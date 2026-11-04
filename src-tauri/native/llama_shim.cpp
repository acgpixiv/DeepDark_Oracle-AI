// llama.cpp 的极简 C ABI 包装层。
//
// 为什么需要它：llama.h 里的 llama_model_params / llama_context_params /
// llama_batch 都是按值传递的 struct，字段顺序随版本变化。在 Rust 里手写
// 这些 struct 的镜像，只要 llama.cpp 升一次版本就可能内存布局错位而崩溃。
// 这里用 C++ 直接包含 llama.h 编译，只向 Rust 暴露 5 个简单函数。

#include "llama.h"
#include "llama-model.h"
#include "mtmd.h"
#include "ggml-backend.h"

#include <cmath>
#include <cctype>
#include <cstddef>
#include <cstdint>
#include <cstdio>
#include <cstring>
#include <algorithm>
#include <atomic>
#include <filesystem>
#include <limits>
#include <mutex>
#include <memory>
#include <optional>
#include <string>
#include <thread>
#include <vector>

namespace {

std::once_flag backend_init_once;
constexpr uint32_t kMaxBatchSequences = 32;
std::mutex model_create_mutex;
std::mutex model_log_mutex;
std::string model_log_messages;

void capture_model_log(ggml_log_level level, const char * text, void *) {
    if (level < GGML_LOG_LEVEL_WARN || !text) {
        return;
    }
    std::lock_guard<std::mutex> lock(model_log_mutex);
    if (model_log_messages.size() < 8192) {
        model_log_messages.append(text, std::min<size_t>(std::strlen(text), 8192 - model_log_messages.size()));
    }
}

std::string take_model_log() {
    std::lock_guard<std::mutex> lock(model_log_mutex);
    std::string result;
    result.swap(model_log_messages);
    return result;
}

void ensure_backend_initialized() {
    std::call_once(backend_init_once, [] {
        // Backend enumeration can itself print device discovery lines. The
        // desktop app reports device state through its own API, so keep the
        // native logger quiet before initialization as well as during model
        // execution.
        llama_log_set([](ggml_log_level, const char *, void *) {}, nullptr);
        llama_backend_init();
    });
}

struct SharedModel {
    std::atomic<uint32_t> references{1};
    llama_model *model = nullptr;
    std::string path;
    std::string backend_device;
};

struct Handle {
    SharedModel *shared = nullptr;
    llama_context *ctx = nullptr;
    mtmd_context *mtmd = nullptr;
    const llama_vocab *vocab = nullptr;
    int input_dim = 0;
    uint32_t max_sequences = 1;
    bool vision = false;
    bool audio = false;
    std::string multimodal_detail = "same-folder mmproj not found";
    int dim = 0;      // 输出向量维度（可能是投影后的 768）
    int n_batch = 0;  // 单次 decode 能接受的最大 token 数（= cparams.n_batch）
};

void retain_shared_model(SharedModel *shared) {
    if (shared) shared->references.fetch_add(1, std::memory_order_relaxed);
}

void release_shared_model(SharedModel *shared) {
    if (!shared || shared->references.fetch_sub(1, std::memory_order_acq_rel) != 1) return;
    if (shared->model) llama_model_free(shared->model);
    delete shared;
}

struct ds_llama_media_item {
    int type; // 1 = RGB image, 2 = mono F32 audio
    const void *data;
    size_t length; // image bytes or audio samples
    uint32_t width;
    uint32_t height;
    int token_budget;
};

void set_err(char *err, int err_len, const char *msg) {
    if (err && err_len > 0) {
        std::snprintf(err, static_cast<size_t>(err_len), "%s", msg ? msg : "unknown error");
    }
}

std::string device_key(ggml_backend_dev_t dev) {
    ggml_backend_dev_props props{};
    ggml_backend_dev_get_props(dev, &props);
    const char *backend = ggml_backend_reg_name(ggml_backend_dev_backend_reg(dev));
    const char *name = ggml_backend_dev_name(dev);
    std::string key = backend ? backend : "unknown";
    key.push_back('|');
    key += props.device_id && *props.device_id ? props.device_id : (name ? name : "unknown");
    return key;
}

std::string lowercase(std::string value) {
    std::transform(value.begin(), value.end(), value.begin(), [](unsigned char ch) {
        return static_cast<char>(std::tolower(ch));
    });
    return value;
}

std::optional<std::filesystem::path> find_mmproj(const char *model_path) {
    std::error_code ec;
    const std::filesystem::path model = std::filesystem::u8path(model_path);
    std::vector<std::filesystem::path> candidates;
    for (std::filesystem::directory_iterator it(model.parent_path(), ec), end; !ec && it != end; it.increment(ec)) {
        if (!it->is_regular_file(ec)) {
            continue;
        }
        const auto path = it->path();
        if (lowercase(path.extension().string()) != ".gguf") {
            continue;
        }
        const auto name = lowercase(path.filename().u8string());
        if (name.find("mmproj") != std::string::npos || name.find("projector") != std::string::npos) {
            candidates.push_back(path);
        }
    }
    if (candidates.empty()) {
        return std::nullopt;
    }
    std::sort(candidates.begin(), candidates.end());
    const auto model_stem = lowercase(model.stem().u8string());
    const auto match = std::find_if(candidates.begin(), candidates.end(), [&](const auto &candidate) {
        return lowercase(candidate.filename().u8string()).find(model_stem) != std::string::npos;
    });
    if (match != candidates.end()) {
        return *match;
    }
    // A single companion can be paired safely even when its filename does not
    // repeat the model stem. With several projectors, guessing would silently
    // attach the wrong vision/audio tower, so report text-only capability.
    return candidates.size() == 1 ? std::optional<std::filesystem::path>(candidates.front())
                                  : std::nullopt;
}

bool append_embedding_row(Handle *h, llama_token token, std::vector<float> &destination) {
    const ggml_tensor *table = h->shared && h->shared->model ? h->shared->model->tok_embd : nullptr;
    if (!table || token < 0 || token >= table->ne[1] || table->ne[0] != h->input_dim) {
        return false;
    }
    const size_t row_bytes = ggml_row_size(table->type, table->ne[0]);
    const size_t offset = static_cast<size_t>(token) * table->nb[1];
    std::vector<uint8_t> raw(row_bytes);
    ggml_backend_tensor_get(table, raw.data(), offset, row_bytes);
    std::vector<float> row(static_cast<size_t>(h->input_dim));
    if (table->type == GGML_TYPE_F32) {
        std::memcpy(row.data(), raw.data(), row_bytes);
    } else {
        const auto *traits = ggml_get_type_traits(table->type);
        if (!traits || !traits->to_float) {
            return false;
        }
        traits->to_float(raw.data(), row.data(), h->input_dim);
    }
    destination.insert(destination.end(), row.begin(), row.end());
    return true;
}

int run_embedding_input(Handle *h, const std::vector<float> &input, float *out,
                        char *err, int err_len) {
    if (input.empty() || input.size() % static_cast<size_t>(h->input_dim) != 0) {
        set_err(err, err_len, "mixed input has invalid embedding width");
        return -1;
    }
    const size_t n_tokens = input.size() / static_cast<size_t>(h->input_dim);
    if (n_tokens > static_cast<size_t>(h->n_batch) || n_tokens > static_cast<size_t>(INT32_MAX)) {
        set_err(err, err_len, "mixed media input exceeds model context");
        return -1;
    }
    llama_batch batch = llama_batch_init(static_cast<int32_t>(n_tokens), h->input_dim, 1);
    if (!batch.embd || !batch.pos || !batch.n_seq_id || !batch.seq_id || !batch.logits) {
        llama_batch_free(batch);
        set_err(err, err_len, "failed to allocate mixed embedding batch");
        return -1;
    }
    batch.n_tokens = static_cast<int32_t>(n_tokens);
    std::memcpy(batch.embd, input.data(), input.size() * sizeof(float));
    for (size_t i = 0; i < n_tokens; ++i) {
        batch.pos[i] = static_cast<llama_pos>(i);
        batch.n_seq_id[i] = 1;
        batch.seq_id[i][0] = 0;
        batch.logits[i] = i + 1 == n_tokens ? 1 : 0;
    }
    const int32_t rc = llama_decode(h->ctx, batch);
    llama_batch_free(batch);
    if (rc != 0) {
        char buf[160];
        std::snprintf(buf, sizeof(buf), "llama_decode mixed media returned %d", rc);
        set_err(err, err_len, buf);
        llama_memory_t mem = llama_get_memory(h->ctx);
        if (mem) llama_memory_clear(mem, true);
        return -1;
    }
    float *embedding = llama_get_embeddings_seq(h->ctx, 0);
    if (!embedding) {
        embedding = llama_get_embeddings(h->ctx);
    }
    if (!embedding) {
        set_err(err, err_len, "EmbeddingGemma2 returned no pooled embedding");
        llama_memory_t mem = llama_get_memory(h->ctx);
        if (mem) llama_memory_clear(mem, true);
        return -1;
    }
    double norm_squared = 0.0;
    for (int i = 0; i < h->dim; ++i) {
        norm_squared += static_cast<double>(embedding[i]) * embedding[i];
    }
    if (!std::isfinite(norm_squared) || norm_squared <= 1e-20) {
        set_err(err, err_len, "EmbeddingGemma2 returned a non-finite or zero vector");
        llama_memory_t mem = llama_get_memory(h->ctx);
        if (mem) llama_memory_clear(mem, true);
        return -1;
    }
    const float inverse_norm = static_cast<float>(1.0 / std::sqrt(norm_squared));
    for (int i = 0; i < h->dim; ++i) {
        out[i] = embedding[i] * inverse_norm;
    }
    llama_memory_t mem = llama_get_memory(h->ctx);
    if (mem) llama_memory_clear(mem, true);
    return h->dim;
}

int run_embedding_inputs(Handle *h, const std::vector<std::vector<float>> &inputs,
                         float *out, int out_stride, char *err, int err_len) {
    if (inputs.empty() || inputs.size() > h->max_sequences || !out || out_stride < h->dim) {
        set_err(err, err_len, "invalid mixed embedding batch arguments");
        return -1;
    }

    size_t total_tokens = 0;
    for (const auto &input : inputs) {
        if (input.empty() || input.size() % static_cast<size_t>(h->input_dim) != 0) {
            set_err(err, err_len, "mixed input has invalid embedding width");
            return -1;
        }
        const size_t tokens = input.size() / static_cast<size_t>(h->input_dim);
        if (tokens > static_cast<size_t>(h->n_batch) ||
            total_tokens > static_cast<size_t>(h->n_batch) - tokens) {
            set_err(err, err_len, "mixed media batch exceeds model batch context");
            return -1;
        }
        total_tokens += tokens;
    }
    if (total_tokens > static_cast<size_t>(INT32_MAX)) {
        set_err(err, err_len, "mixed media batch token count overflow");
        return -1;
    }

    llama_batch batch = llama_batch_init(static_cast<int32_t>(total_tokens), h->input_dim,
                                         static_cast<int32_t>(inputs.size()));
    if (!batch.embd || !batch.pos || !batch.n_seq_id || !batch.seq_id || !batch.logits) {
        llama_batch_free(batch);
        set_err(err, err_len, "failed to allocate mixed embedding batch");
        return -1;
    }
    batch.n_tokens = static_cast<int32_t>(total_tokens);
    size_t cursor = 0;
    for (size_t seq = 0; seq < inputs.size(); ++seq) {
        const auto &input = inputs[seq];
        const size_t tokens = input.size() / static_cast<size_t>(h->input_dim);
        std::memcpy(batch.embd + cursor * static_cast<size_t>(h->input_dim),
                    input.data(), input.size() * sizeof(float));
        for (size_t pos = 0; pos < tokens; ++pos, ++cursor) {
            batch.pos[cursor] = static_cast<llama_pos>(pos);
            batch.n_seq_id[cursor] = 1;
            batch.seq_id[cursor][0] = static_cast<llama_seq_id>(seq);
            batch.logits[cursor] = pos + 1 == tokens ? 1 : 0;
        }
    }

    const int32_t rc = llama_decode(h->ctx, batch);
    llama_batch_free(batch);
    if (rc != 0) {
        char detail[160];
        std::snprintf(detail, sizeof(detail), "llama_decode mixed media batch returned %d", rc);
        set_err(err, err_len, detail);
        llama_memory_t mem = llama_get_memory(h->ctx);
        if (mem) llama_memory_clear(mem, true);
        return -1;
    }

    for (size_t seq = 0; seq < inputs.size(); ++seq) {
        float *embedding = llama_get_embeddings_seq(h->ctx, static_cast<llama_seq_id>(seq));
        if (!embedding) {
            set_err(err, err_len, "EmbeddingGemma2 returned no mixed sequence embedding");
            llama_memory_t mem = llama_get_memory(h->ctx);
            if (mem) llama_memory_clear(mem, true);
            return -1;
        }
        double norm_squared = 0.0;
        for (int i = 0; i < h->dim; ++i) {
            norm_squared += static_cast<double>(embedding[i]) * embedding[i];
        }
        if (!std::isfinite(norm_squared) || norm_squared <= 1e-20) {
            set_err(err, err_len, "EmbeddingGemma2 returned a non-finite or zero mixed vector");
            llama_memory_t mem = llama_get_memory(h->ctx);
            if (mem) llama_memory_clear(mem, true);
            return -1;
        }
        const float inverse_norm = static_cast<float>(1.0 / std::sqrt(norm_squared));
        float *destination = out + seq * static_cast<size_t>(out_stride);
        for (int i = 0; i < h->dim; ++i) {
            destination[i] = embedding[i] * inverse_norm;
        }
    }

    llama_memory_t mem = llama_get_memory(h->ctx);
    if (mem) llama_memory_clear(mem, true);
    return static_cast<int>(inputs.size());
}

struct MtmdChunksDeleter {
    void operator()(mtmd_input_chunks *chunks) const {
        if (chunks) mtmd_input_chunks_free(chunks);
    }
};

struct MtmdBatchDeleter {
    void operator()(mtmd_batch *batch) const {
        if (batch) mtmd_batch_free(batch);
    }
};

struct MediaSequence {
    std::unique_ptr<mtmd_input_chunks, MtmdChunksDeleter> chunks;
    const mtmd_input_chunk *media_chunk = nullptr;
    std::vector<float> projected;
};

bool encode_media_batch(Handle *h, std::vector<MediaSequence *> sequences,
                        char *err, int err_len) {
    if (sequences.empty()) {
        return true;
    }
    std::unique_ptr<mtmd_batch, MtmdBatchDeleter> batch(mtmd_batch_init(h->mtmd));
    if (!batch) {
        set_err(err, err_len, "failed to allocate mtmd media batch");
        return false;
    }
    std::vector<MediaSequence *> accepted;
    accepted.reserve(sequences.size());
    for (MediaSequence *sequence : sequences) {
        if (!sequence || !sequence->media_chunk) {
            set_err(err, err_len, "invalid mtmd media sequence");
            return false;
        }
        const int32_t add = mtmd_batch_add_chunk(batch.get(), sequence->media_chunk);
        if (add == 0) {
            accepted.push_back(sequence);
            continue;
        }
        if (add != 2 && add != 3) {
            set_err(err, err_len, "mtmd rejected a media batch chunk");
            return false;
        }

        // A projector may not support batching, or the image shape/token
        // budget may differ. Flush the compatible prefix and retry the
        // current chunk in a fresh batch. This preserves correctness for
        // older mmproj files while allowing supported encoders to run one
        // GPU graph for many images.
        if (!accepted.empty()) {
            if (mtmd_batch_encode(batch.get()) != 0) {
                set_err(err, err_len, "mmproj failed to encode a media batch");
                return false;
            }
            for (MediaSequence *item : accepted) {
                const size_t token_count = mtmd_input_chunk_get_n_tokens(item->media_chunk);
                const float *output = mtmd_batch_get_output_embd(batch.get(), item->media_chunk);
                if (!output || token_count == 0) {
                    set_err(err, err_len, "mmproj returned an empty media batch output");
                    return false;
                }
                item->projected.assign(output,
                    output + token_count * static_cast<size_t>(h->input_dim));
            }
            batch.reset(mtmd_batch_init(h->mtmd));
            if (!batch) {
                set_err(err, err_len, "failed to allocate mtmd media batch");
                return false;
            }
            accepted.clear();
        }
        const int32_t retry = mtmd_batch_add_chunk(batch.get(), sequence->media_chunk);
        if (retry != 0) {
            set_err(err, err_len, "mmproj cannot encode this media input");
            return false;
        }
        accepted.push_back(sequence);
    }

    if (!accepted.empty()) {
        if (mtmd_batch_encode(batch.get()) != 0) {
            set_err(err, err_len, "mmproj failed to encode the final media batch");
            return false;
        }
        for (MediaSequence *item : accepted) {
            const size_t token_count = mtmd_input_chunk_get_n_tokens(item->media_chunk);
            const float *output = mtmd_batch_get_output_embd(batch.get(), item->media_chunk);
            if (!output || token_count == 0) {
                set_err(err, err_len, "mmproj returned an empty media batch output");
                return false;
            }
            item->projected.assign(output,
                output + token_count * static_cast<size_t>(h->input_dim));
        }
    }
    return true;
}

bool prepare_media_sequence(Handle *h, const ds_llama_media_item &item,
                            MediaSequence &sequence,
                            char *err, int err_len) {
    if (!item.data || item.length == 0) {
        set_err(err, err_len, "media item is empty");
        return false;
    }
    mtmd_bitmap *bitmap = nullptr;
    if (item.type == 1) {
        if (!h->vision || item.width == 0 || item.height == 0 ||
            item.length != static_cast<size_t>(item.width) * item.height * 3) {
            set_err(err, err_len, "GGUF mmproj does not support this RGB image input");
            return false;
        }
        bitmap = mtmd_bitmap_init(item.width, item.height,
                                  static_cast<const unsigned char *>(item.data));
    } else if (item.type == 2) {
        if (!h->audio) {
            set_err(err, err_len, "GGUF mmproj does not include an audio encoder");
            return false;
        }
        bitmap = mtmd_bitmap_init_from_audio(item.length,
                                             static_cast<const float *>(item.data));
    } else {
        set_err(err, err_len, "unknown GGUF media type");
        return false;
    }
    if (!bitmap) {
        set_err(err, err_len, "failed to allocate mtmd bitmap");
        return false;
    }

    const char *marker = mtmd_get_marker(h->mtmd);
    std::string prompt = "title: none | text: ";
    prompt += marker ? marker : "<|image|>";
    prompt.push_back('\n');
    mtmd_input_text text{prompt.c_str(), prompt.size(), true, true};
    const mtmd_bitmap *view = bitmap;
    auto *chunks = mtmd_input_chunks_init();
    if (!chunks) {
        mtmd_bitmap_free(bitmap);
        set_err(err, err_len, "failed to allocate mtmd chunks");
        return false;
    }
    const int32_t tokenized = mtmd_tokenize(h->mtmd, chunks, &text, &view, 1);
    mtmd_bitmap_free(bitmap);
    if (tokenized != 0) {
        mtmd_input_chunks_free(chunks);
        set_err(err, err_len, "mtmd_tokenize failed to prepare media input");
        return false;
    }
    sequence.chunks.reset(chunks);
    for (size_t i = 0; i < mtmd_input_chunks_size(chunks); ++i) {
        const mtmd_input_chunk *chunk = mtmd_input_chunks_get(chunks, i);
        if (mtmd_input_chunk_get_type(chunk) != MTMD_INPUT_CHUNK_TYPE_TEXT) {
            if (sequence.media_chunk) {
                set_err(err, err_len, "this media input requires multiple chunks; use the mixed media path");
                return false;
            }
            sequence.media_chunk = chunk;
        }
    }
    if (!sequence.media_chunk) {
        set_err(err, err_len, "mtmd did not return a media chunk");
        return false;
    }
    return true;
}

bool build_media_embedding_input(Handle *h, MediaSequence &sequence,
                                 std::vector<float> &input,
                                 char *err, int err_len) {
    input.clear();
    const auto *chunks = sequence.chunks.get();
    if (!chunks || !sequence.media_chunk || sequence.projected.empty()) {
        set_err(err, err_len, "media sequence is not encoded");
        return false;
    }
    for (size_t i = 0; i < mtmd_input_chunks_size(chunks); ++i) {
        const mtmd_input_chunk *chunk = mtmd_input_chunks_get(chunks, i);
        if (mtmd_input_chunk_get_type(chunk) == MTMD_INPUT_CHUNK_TYPE_TEXT) {
            size_t count = 0;
            const llama_token *tokens = mtmd_input_chunk_get_tokens_text(chunk, &count);
            if (!tokens) {
                set_err(err, err_len, "mtmd returned invalid media text tokens");
                return false;
            }
            for (size_t j = 0; j < count; ++j) {
                if (input.size() / static_cast<size_t>(h->input_dim) >=
                        static_cast<size_t>(h->n_batch) ||
                    !append_embedding_row(h, tokens[j], input)) {
                    set_err(err, err_len, "failed to read media prompt token embedding");
                    return false;
                }
            }
        } else if (chunk == sequence.media_chunk) {
            const size_t count = sequence.projected.size() / static_cast<size_t>(h->input_dim);
            if (input.size() / static_cast<size_t>(h->input_dim) + count >
                    static_cast<size_t>(h->n_batch)) {
                set_err(err, err_len, "media sequence exceeds model context");
                return false;
            }
            input.insert(input.end(), sequence.projected.begin(), sequence.projected.end());
        }
    }
    return !input.empty();
}

}  // namespace

extern "C" {

// Enumerate devices from the exact GGML backends linked into this build. This
// deliberately excludes CPU/BLAS devices: they are not accelerators that can
// satisfy a user's GPU selection. CUDA and Vulkan builds expose only devices
// supported by their linked backends.
int ds_llama_device_count() {
    ensure_backend_initialized();
    int count = 0;
    for (size_t i = 0; i < ggml_backend_dev_count(); ++i) {
        const auto dev = ggml_backend_dev_get(i);
        const auto type = ggml_backend_dev_type(dev);
        if (type == GGML_BACKEND_DEVICE_TYPE_GPU || type == GGML_BACKEND_DEVICE_TYPE_IGPU) {
            ++count;
        }
    }
    return count;
}

int ds_llama_device_info(int wanted, char *name, int name_len, char *description,
                         int description_len, char *device_id, int device_id_len,
                         char *physical_id, int physical_id_len,
                         char *backend, int backend_len, std::uint64_t *free_bytes,
                         std::uint64_t *total_bytes, int *type) {
    ensure_backend_initialized();
    if (wanted < 0 || !name || name_len <= 0 || !description || description_len <= 0 ||
        !device_id || device_id_len <= 0 || !physical_id || physical_id_len <= 0 ||
        !backend || backend_len <= 0) {
        return -1;
    }
    int current = 0;
    for (size_t i = 0; i < ggml_backend_dev_count(); ++i) {
        const auto dev = ggml_backend_dev_get(i);
        const auto dev_type = ggml_backend_dev_type(dev);
        if (dev_type != GGML_BACKEND_DEVICE_TYPE_GPU && dev_type != GGML_BACKEND_DEVICE_TYPE_IGPU) {
            continue;
        }
        if (current++ != wanted) {
            continue;
        }
        const char *dev_name = ggml_backend_dev_name(dev);
        const char *dev_desc = ggml_backend_dev_description(dev);
        ggml_backend_dev_props props{};
        ggml_backend_dev_get_props(dev, &props);
        const char *reg_name = ggml_backend_reg_name(ggml_backend_dev_backend_reg(dev));
        std::snprintf(name, static_cast<size_t>(name_len), "%s", dev_name ? dev_name : "");
        std::snprintf(description, static_cast<size_t>(description_len), "%s", dev_desc ? dev_desc : "");
        const auto key = device_key(dev);
        std::snprintf(device_id, static_cast<size_t>(device_id_len), "%s", key.c_str());
        std::snprintf(physical_id, static_cast<size_t>(physical_id_len), "%s", props.device_id ? props.device_id : "");
        std::snprintf(backend, static_cast<size_t>(backend_len), "%s", reg_name ? reg_name : "");
        size_t free_mem = 0;
        size_t total_mem = 0;
        ggml_backend_dev_memory(dev, &free_mem, &total_mem);
        if (free_bytes) {
            *free_bytes = static_cast<std::uint64_t>(free_mem);
        }
        if (total_bytes) {
            *total_bytes = static_cast<std::uint64_t>(total_mem);
        }
        if (type) {
            *type = dev_type == GGML_BACKEND_DEVICE_TYPE_IGPU ? 1 : 0;
        }
        return 0;
    }
    return -1;
}

// 创建句柄。返回 nullptr 表示失败，错误信息写入 err。
void *ds_llama_create(const char *model_path, int n_ctx, int n_threads,
                      const char *device_name, char *err, int err_len) {
    if (!model_path || !*model_path) {
        set_err(err, err_len, "模型路径为空");
        return nullptr;
    }

    ensure_backend_initialized();
    // llama.cpp 默认把 graph/内存布局逐行打到 stderr（一次编码上千行），
    // 桌面端没有控制台，这些日志会拖慢编码并污染日志文件，直接静音。
    std::lock_guard<std::mutex> create_lock(model_create_mutex);
    take_model_log();
    llama_log_set(capture_model_log, nullptr);

    llama_model_params mparams = llama_model_default_params();
    // 显存自适应降级（REQ-097）。
    //
    // 旧写法无条件 `n_gpu_layers = -1`（全量下放），有两个问题：
    //   1. 用户显式选择 CPU（上层传 nullptr）时，-1 仍会让 llama.cpp 自己
    //      挑一块 GPU 用，等于无视了「这台机器别用显卡」的选择；
    //   2. 集显/小显存机器上整模型下放会直接 OOM，模型加载失败而不是降级。
    //
    // 现在的策略是：没指定设备 → 0 层（纯 CPU）；指定了设备 → 按该设备
    // 的总显存分档，< 1 GiB 视为 Tier 3 退回 CPU，其余仍全量下放（嵌入模型
    // 是几百 MB 量级，量化后 4 GiB 以上显存足够）。
    mparams.n_gpu_layers = 0;
    ggml_backend_dev_t selected_devices[2] = { nullptr, nullptr };
    ggml_backend_dev_t selected_device = nullptr;
    if (device_name && *device_name) {
        ggml_backend_dev_t selected = nullptr;
        for (size_t i = 0; i < ggml_backend_dev_count(); ++i) {
            auto candidate = ggml_backend_dev_get(i);
            const auto type = ggml_backend_dev_type(candidate);
            if ((type == GGML_BACKEND_DEVICE_TYPE_GPU || type == GGML_BACKEND_DEVICE_TYPE_IGPU) &&
                device_key(candidate) == device_name) {
                selected = candidate;
                break;
            }
        }
        if (!selected) {
            set_err(err, err_len, "所选显卡不在当前后端支持的设备列表中，请重新选择设备");
            return nullptr;
        }
        const auto type = ggml_backend_dev_type(selected);
        if (type != GGML_BACKEND_DEVICE_TYPE_GPU && type != GGML_BACKEND_DEVICE_TYPE_IGPU) {
            set_err(err, err_len, "指定设备不是 GPU 或集成显卡");
            return nullptr;
        }
        // 显存分档（REQ-097）：Tier 0~2 全量下放，Tier 3（<1 GiB）退回 CPU。
        //
        // 嵌入模型只有几百 MB，真正吃显存的是 KV cache 与视觉塔。1 GiB 是
        // 「连权重都放不下」的经验阈值，低于它强行下放只会让
        // llama_model_load_from_file 直接失败，而失败信息又长得像模型损坏，
        // 用户根本猜不到是显存不够。宁可先跑 CPU，也不要报假错。
        ggml_backend_dev_props selected_props{};
        ggml_backend_dev_get_props(selected, &selected_props);
        constexpr size_t kTier3Vram = 1ull * 1024ull * 1024ull * 1024ull;
        if (selected_props.memory_total > 0 && selected_props.memory_total < kTier3Vram) {
            // Tier 3：显存不足 1 GiB，等于没有可用的加速器，走 CPU。
            selected_device = nullptr;
        } else {
            selected_device = selected;
            // -1 = 把全部模型层交给选中的加速器（llama.cpp 的约定）。
            mparams.n_gpu_layers = -1;
            selected_devices[0] = selected;
            mparams.devices = selected_devices;
            mparams.split_mode = LLAMA_SPLIT_MODE_NONE;
            mparams.main_gpu = 0;
        }
    }

    llama_model *model = llama_model_load_from_file(model_path, mparams);
    if (!model) {
        const auto detail = take_model_log();
        set_err(err, err_len, detail.empty()
            ? "llama_model_load_from_file failed; verify the GGUF architecture and tensor layout"
            : detail.c_str());
        return nullptr;
    }

    llama_context_params cparams = llama_context_default_params();
    cparams.n_ctx = static_cast<uint32_t>(n_ctx > 0 ? n_ctx : 2048);
    // 新版 llama.cpp 对 encoder 模型有硬断言：
    //   GGML_ASSERT(cparams.n_ubatch >= n_tokens && "encoder requires n_ubatch >= n_tokens")
    // 一旦传入的文本 token 数超过 n_ubatch，进程会被直接 abort（不是返回错误码），
    // 表现为「索引到某个长文档时窗口突然消失」。这里把 batch 拉到上下文长度上限，
    // 并在 ds_llama_embed 里按 n_batch 截断，保证永远不会越界。
    const uint32_t sequence_context = std::max<uint32_t>(cparams.n_ctx, 512u);
    // Encoder-style embedding models have no KV cache and can share one
    // context across a larger number of independent sequence IDs. Causal
    // GGUF models retain a conservative sequence count so we do not silently
    // allocate dozens of full KV contexts.
    const bool non_causal = !model->hparams.causal_attn;
    const uint32_t max_sequences = non_causal ? kMaxBatchSequences : 4u;
    cparams.n_seq_max = max_sequences;
    // Non-causal EmbeddingGemma has no KV cache, so one 8192-token budget can
    // be shared by all sequence IDs. Causal models retain a per-sequence
    // context and a smaller sequence cap to bound KV memory.
    cparams.kv_unified = non_causal;
    cparams.n_ctx = non_causal ? sequence_context : sequence_context * max_sequences;
    cparams.n_batch = sequence_context;
    cparams.n_ubatch = sequence_context;
    // A zero thread setting means "automatic". The previous fallback of four
    // threads left tokenisation, prompt preparation and CPU-side kernels
    // underutilised on ordinary desktop CPUs. Keep an explicit user value
    // authoritative, otherwise use all logical cores and give batched work a
    // little more room to overlap host-side preparation with GPU execution.
    const int automatic_threads = static_cast<int>(std::max(1u, std::thread::hardware_concurrency()));
    const int threads = n_threads > 0 ? n_threads : automatic_threads;
    cparams.n_threads = threads;
    cparams.n_threads_batch = n_threads > 0
        ? n_threads
        : std::max(threads, std::min(threads * 2, 64));
    cparams.embeddings = true;
    cparams.pooling_type = LLAMA_POOLING_TYPE_MEAN;
    cparams.offload_kqv = true;

    mtmd_context *mmproj = nullptr;
    std::string multimodal_detail = "同目录未找到 mmproj，当前仅支持文本";
    const auto mmproj_path = find_mmproj(model_path);
    if (mmproj_path) {
        mtmd_context_params mmparams = mtmd_context_params_default();
        mmparams.use_gpu = selected_device != nullptr;
        // llama.cpp 0.7 把 mtmd_context_params::backend_device（设备名字符串）
        // 换成了 ggml_backend_dev_t device 句柄；传 nullptr 表示使用默认设备。
        mmparams.device = selected_device;
        mmparams.print_timings = false;
        mmparams.warmup = false;
        mmparams.n_threads = threads;
        mmparams.image_max_tokens = 1120;
        mmparams.batch_max_tokens = static_cast<int32_t>(cparams.n_batch);
        mtmd_log_set(capture_model_log, nullptr);
        const auto mmproj_utf8_path = mmproj_path->u8string();
        mmproj = mtmd_init_from_file(mmproj_utf8_path.c_str(), model, mmparams);
        if (mmproj) {
            const bool has_vision = mtmd_support_vision(mmproj);
            const bool has_audio = mtmd_support_audio(mmproj);
            if (has_vision || has_audio) {
                multimodal_detail = "已加载配套 mmproj";
                if (has_vision) multimodal_detail += "，支持图像及视频抽帧";
                if (has_audio) multimodal_detail += "，支持音频";
            } else {
                mtmd_free(mmproj);
                mmproj = nullptr;
                multimodal_detail = "mmproj 未声明可用的图像或音频编码器";
            }
        } else {
            const auto detail = take_model_log();
            multimodal_detail = "mmproj 加载失败";
            if (!detail.empty()) multimodal_detail += "：" + detail;
        }
    }

    llama_context *ctx = llama_init_from_model(model, cparams);
    if (!ctx) {
        if (mmproj) mtmd_free(mmproj);
        llama_model_free(model);
        const auto detail = take_model_log();
        set_err(err, err_len, detail.empty()
            ? "llama_init_from_model failed; available GPU memory or context settings may be insufficient"
            : detail.c_str());
        return nullptr;
    }

    auto *h = new Handle();
    auto *shared = new SharedModel();
    shared->model = model;
    shared->path = model_path;
    if (selected_device) shared->backend_device = ggml_backend_dev_name(selected_device);
    h->shared = shared;
    h->ctx = ctx;
    h->mtmd = mmproj;
    h->vocab = llama_model_get_vocab(model);
    h->input_dim = llama_model_n_embd_inp(model);
    h->max_sequences = max_sequences;
    h->vision = mmproj && mtmd_support_vision(mmproj);
    h->audio = mmproj && mtmd_support_audio(mmproj);
    h->multimodal_detail = std::move(multimodal_detail);
    // 注意：要用 **输出** 维度，不是隐层维度。
    // EmbeddingGemma 2 的 embedding_length = 512、embedding_length_out = 768；
    // llama_get_embeddings* 返回的是投影后的 768 维向量，若按 512 拷贝就会
    // 越界写坏堆，表现为「用一会儿莫名崩溃 0xc0000409 in ucrtbase.dll」。
    h->n_batch = static_cast<int>(cparams.n_batch);
    h->dim = llama_model_n_embd_out(model);
    if (h->dim <= 0) {
        h->dim = llama_model_n_embd(model);
    }
    if (h->dim <= 0) {
        llama_free(ctx);
        release_shared_model(shared);
        delete h;
        set_err(err, err_len, "模型未报告 embedding 维度");
        return nullptr;
    }
    return h;
}

// Create a second execution context against the same immutable llama_model.
// Every context has independent llama/mtmd state; only the read-only model
// weights are shared. This function never serializes inference calls.
void *ds_llama_duplicate_context(void *source, int n_ctx, int n_threads,
                                 int with_multimodal, char *err, int err_len) {
    auto *parent = static_cast<Handle *>(source);
    if (!parent || !parent->shared || !parent->shared->model) {
        set_err(err, err_len, "shared GGUF model handle is invalid");
        return nullptr;
    }
    auto *shared = parent->shared;
    auto *model = shared->model;
    const int threads = n_threads > 0
        ? n_threads
        : static_cast<int>(std::max(1u, std::thread::hardware_concurrency()));

    llama_context_params cparams = llama_context_default_params();
    cparams.n_ctx = static_cast<uint32_t>(n_ctx > 0 ? n_ctx : 2048);
    const uint32_t sequence_context = std::max<uint32_t>(cparams.n_ctx, 512u);
    const bool non_causal = !model->hparams.causal_attn;
    const uint32_t max_sequences = non_causal ? kMaxBatchSequences : 4u;
    cparams.n_seq_max = max_sequences;
    cparams.kv_unified = non_causal;
    cparams.n_ctx = non_causal ? sequence_context : sequence_context * max_sequences;
    cparams.n_batch = sequence_context;
    cparams.n_ubatch = sequence_context;
    cparams.n_threads = threads;
    cparams.n_threads_batch = n_threads > 0
        ? n_threads
        : std::max(threads, std::min(threads * 2, 64));
    cparams.embeddings = true;
    cparams.pooling_type = LLAMA_POOLING_TYPE_MEAN;
    cparams.offload_kqv = true;

    mtmd_context *mmproj = nullptr;
    std::string detail = "text context; multimodal projection disabled";
    if (with_multimodal) {
        ggml_backend_dev_t selected_device = nullptr;
        if (!shared->backend_device.empty()) {
            for (size_t i = 0; i < ggml_backend_dev_count(); ++i) {
                auto candidate = ggml_backend_dev_get(i);
                if (shared->backend_device == ggml_backend_dev_name(candidate)) {
                    selected_device = candidate;
                    break;
                }
            }
        }
        const auto mmproj_path = find_mmproj(shared->path.c_str());
        detail = "same-folder mmproj not found; text only";
        if (mmproj_path) {
            mtmd_context_params mmparams = mtmd_context_params_default();
            mmparams.use_gpu = selected_device != nullptr;
            mmparams.device = selected_device;
            mmparams.print_timings = false;
            mmparams.warmup = false;
            mmparams.n_threads = threads;
            mmparams.image_max_tokens = 1120;
            mmparams.batch_max_tokens = static_cast<int32_t>(cparams.n_batch);
            mtmd_log_set(capture_model_log, nullptr);
            const auto path = mmproj_path->u8string();
            mmproj = mtmd_init_from_file(path.c_str(), model, mmparams);
            if (mmproj) {
                const bool vision = mtmd_support_vision(mmproj);
                const bool audio = mtmd_support_audio(mmproj);
                if (vision || audio) {
                    detail = "mmproj loaded";
                    if (vision) detail += "; image and video frames supported";
                    if (audio) detail += "; audio supported";
                } else {
                    mtmd_free(mmproj);
                    mmproj = nullptr;
                    detail = "mmproj has no usable image or audio encoder";
                }
            } else {
                const auto log = take_model_log();
                detail = "mmproj load failed";
                if (!log.empty()) detail += ": " + log;
            }
        }
    }

    llama_context *ctx = llama_init_from_model(model, cparams);
    if (!ctx) {
        if (mmproj) mtmd_free(mmproj);
        const auto log = take_model_log();
        set_err(err, err_len, log.empty()
            ? "llama_init_from_model failed for shared model context"
            : log.c_str());
        return nullptr;
    }

    auto *h = new Handle();
    h->shared = shared;
    retain_shared_model(shared);
    h->ctx = ctx;
    h->mtmd = mmproj;
    h->vocab = llama_model_get_vocab(model);
    h->input_dim = llama_model_n_embd_inp(model);
    h->max_sequences = max_sequences;
    h->vision = mmproj && mtmd_support_vision(mmproj);
    h->audio = mmproj && mtmd_support_audio(mmproj);
    h->multimodal_detail = std::move(detail);
    h->n_batch = static_cast<int>(cparams.n_batch);
    h->dim = llama_model_n_embd_out(model);
    if (h->dim <= 0) h->dim = llama_model_n_embd(model);
    if (h->dim <= 0) {
        llama_free(ctx);
        if (mmproj) mtmd_free(mmproj);
        release_shared_model(shared);
        delete h;
        set_err(err, err_len, "model did not report an embedding dimension");
        return nullptr;
    }
    return h;
}

int ds_llama_dim(void *handle) {
    auto *h = static_cast<Handle *>(handle);
    return h ? h->dim : 0;
}

int ds_llama_capabilities(void *handle) {
    auto *h = static_cast<Handle *>(handle);
    if (!h) return 0;
    int result = 1; // text
    if (h->vision) result |= 2 | 8; // image and frame-based video
    if (h->audio) result |= 4;
    return result;
}

void ds_llama_detail(void *handle, char *out, int out_len) {
    auto *h = static_cast<Handle *>(handle);
    if (!h || !out || out_len <= 0) return;
    std::snprintf(out, static_cast<size_t>(out_len), "%s", h->multimodal_detail.c_str());
}

int ds_llama_audio_sample_rate(void *handle) {
    auto *h = static_cast<Handle *>(handle);
    return h && h->mtmd && h->audio ? mtmd_get_audio_sample_rate(h->mtmd) : -1;
}

int ds_llama_embed_media(void *handle, const ds_llama_media_item *items, int count,
                         float *out, int out_len, char *err, int err_len) {
    auto *h = static_cast<Handle *>(handle);
    if (!h || !h->ctx || !h->mtmd || !items || count <= 0 || !out || out_len < h->dim) {
        set_err(err, err_len, "GGUF mmproj handle or media arguments are invalid");
        return -1;
    }
    std::vector<mtmd_bitmap *> bitmaps;
    bitmaps.reserve(static_cast<size_t>(count));
    std::string prompt = "title: none | text: ";
    int image_budget = 0;
    auto free_bitmaps = [&] {
        for (auto *bitmap : bitmaps) mtmd_bitmap_free(bitmap);
        bitmaps.clear();
    };
    for (int i = 0; i < count; ++i) {
        const auto &item = items[i];
        mtmd_bitmap *bitmap = nullptr;
        if (!item.data || item.length == 0) {
            free_bitmaps();
            set_err(err, err_len, "media item is empty");
            return -1;
        }
        if (item.type == 1) {
            if (!h->vision || item.width == 0 || item.height == 0 ||
                item.length != static_cast<size_t>(item.width) * item.height * 3) {
                free_bitmaps();
                set_err(err, err_len, "GGUF mmproj does not support this RGB image input");
                return -1;
            }
            if (item.token_budget > 0) {
                if (image_budget != 0 && image_budget != item.token_budget) {
                    free_bitmaps();
                    set_err(err, err_len, "one mixed request must use a single image token budget");
                    return -1;
                }
                image_budget = item.token_budget;
            }
            bitmap = mtmd_bitmap_init(item.width, item.height,
                                      static_cast<const unsigned char *>(item.data));
        } else if (item.type == 2) {
            if (!h->audio) {
                free_bitmaps();
                set_err(err, err_len, "GGUF mmproj does not include an audio encoder");
                return -1;
            }
            bitmap = mtmd_bitmap_init_from_audio(item.length, static_cast<const float *>(item.data));
        } else {
            free_bitmaps();
            set_err(err, err_len, "unknown GGUF media type");
            return -1;
        }
        if (!bitmap) {
            free_bitmaps();
            set_err(err, err_len, "failed to allocate mtmd bitmap");
            return -1;
        }
        bitmaps.push_back(bitmap);
        prompt += mtmd_get_marker(h->mtmd);
        prompt.push_back('\n');
    }
    // llama.cpp 0.7 移除了 mtmd_set_image_max_tokens()：图像 token 预算现在
    // 只能在 mtmd_context_params.image_max_tokens 里于初始化时确定。这里保留
    // image_budget 的聚合校验（同一请求不允许混用不同预算），但不再在运行期
    // 改写上下文；视觉编码器的实际 token 数由 mmproj 自身的分辨率与 merge
    // 因子决定，越界仍由下面的 h->n_batch 检查兜底。
    (void) image_budget;
    mtmd_input_chunks *chunks = mtmd_input_chunks_init();
    if (!chunks) {
        free_bitmaps();
        set_err(err, err_len, "failed to allocate mtmd chunks");
        return -1;
    }
    mtmd_input_text text{prompt.c_str(), prompt.size(), true, true};
    std::vector<const mtmd_bitmap *> bitmap_views;
    bitmap_views.reserve(bitmaps.size());
    for (auto *bitmap : bitmaps) bitmap_views.push_back(bitmap);
    const int32_t tokenize_result = mtmd_tokenize(
        h->mtmd, chunks, &text, bitmap_views.data(), bitmap_views.size());
    free_bitmaps();
    if (tokenize_result != 0) {
        mtmd_input_chunks_free(chunks);
        set_err(err, err_len, "mtmd_tokenize failed to prepare the mixed media sequence");
        return -1;
    }
    std::vector<float> input;
    for (size_t index = 0; index < mtmd_input_chunks_size(chunks); ++index) {
        const mtmd_input_chunk *chunk = mtmd_input_chunks_get(chunks, index);
        const auto type = mtmd_input_chunk_get_type(chunk);
        if (type == MTMD_INPUT_CHUNK_TYPE_TEXT) {
            size_t token_count = 0;
            const llama_token *tokens = mtmd_input_chunk_get_tokens_text(chunk, &token_count);
            if (!tokens) {
                mtmd_input_chunks_free(chunks);
                set_err(err, err_len, "mtmd returned an invalid text chunk");
                return -1;
            }
            for (size_t token = 0; token < token_count; ++token) {
                if (input.size() / static_cast<size_t>(h->input_dim) >= static_cast<size_t>(h->n_batch) ||
                    !append_embedding_row(h, tokens[token], input)) {
                    mtmd_input_chunks_free(chunks);
                    set_err(err, err_len, "failed to read a GGUF token embedding row or context is full");
                    return -1;
                }
            }
        } else {
            if (mtmd_encode_chunk(h->mtmd, chunk) != 0) {
                mtmd_input_chunks_free(chunks);
                set_err(err, err_len, "mmproj failed to encode a media chunk");
                return -1;
            }
            const size_t token_count = mtmd_input_chunk_get_n_tokens(chunk);
            const float *media = mtmd_get_output_embd(h->mtmd);
            const size_t current_tokens = input.size() / static_cast<size_t>(h->input_dim);
            if (!media || token_count > static_cast<size_t>(h->n_batch) - current_tokens) {
                mtmd_input_chunks_free(chunks);
                char detail[200];
                std::snprintf(detail, sizeof(detail),
                    "projected media exceeds model context: chunk=%zu accumulated=%zu limit=%d data=%s",
                    token_count, current_tokens, h->n_batch, media ? "present" : "missing");
                set_err(err, err_len, detail);
                return -1;
            }
            input.insert(input.end(), media, media + token_count * static_cast<size_t>(h->input_dim));
        }
    }
    mtmd_input_chunks_free(chunks);
    return run_embedding_input(h, input, out, err, err_len);
}

// Each item is an independent image, not a frame in one shared video sequence.
// Project compatible images together, then pool separate text-model sequences.
int ds_llama_embed_media_batch(void *handle, const ds_llama_media_item *items, int count,
                               float *out, int out_stride, char *err, int err_len) {
    auto *h = static_cast<Handle *>(handle);
    if (!h || !h->ctx || !h->mtmd || !items || count <= 0 ||
        count > static_cast<int>(h->max_sequences) || !out || out_stride < h->dim) {
        set_err(err, err_len, "GGUF mmproj handle or media batch arguments are invalid");
        return -1;
    }
    std::vector<MediaSequence> sequences(static_cast<size_t>(count));
    for (int i = 0; i < count; ++i) {
        if (items[i].type != 1) {
            set_err(err, err_len, "independent media batching currently accepts RGB images only");
            return -1;
        }
        // 同 ds_llama_embed_media：新版 mtmd 取消了运行期的图像 token 上限设置，
        // 每张图的 token_budget 由调用方通过分批数量控制（见 llama.rs 的 per_image），
        // 这里只需保证单张序列仍在 h->n_batch 之内。
        if (!prepare_media_sequence(h, items[i], sequences[static_cast<size_t>(i)], err, err_len)) {
            return -1;
        }
    }
    std::vector<MediaSequence *> pending;
    pending.reserve(sequences.size());
    for (auto &sequence : sequences) pending.push_back(&sequence);
    if (!encode_media_batch(h, pending, err, err_len)) {
        return -1;
    }

    std::vector<std::vector<float>> inputs;
    inputs.reserve(sequences.size());
    size_t tokens = 0;
    int output_offset = 0;
    for (auto &sequence : sequences) {
        std::vector<float> input;
        if (!build_media_embedding_input(h, sequence, input, err, err_len)) {
            return -1;
        }
        const size_t item_tokens = input.size() / static_cast<size_t>(h->input_dim);
        if (!inputs.empty() && tokens + item_tokens > static_cast<size_t>(h->n_batch)) {
            const int written = run_embedding_inputs(h, inputs,
                out + static_cast<size_t>(output_offset) * out_stride, out_stride, err, err_len);
            if (written < 0) return -1;
            output_offset += written;
            inputs.clear();
            tokens = 0;
        }
        tokens += item_tokens;
        inputs.push_back(std::move(input));
    }
    if (!inputs.empty()) {
        const int written = run_embedding_inputs(h, inputs,
            out + static_cast<size_t>(output_offset) * out_stride, out_stride, err, err_len);
        if (written < 0) return -1;
        output_offset += written;
    }
    return output_offset;
}

// 对一段文本求 embedding。is_query != 0 时按查询侧处理。
// out 由调用方分配，长度必须 >= dim；返回实际写入的维度，<0 表示失败。
int ds_llama_embed(void *handle, const char *text, int is_query, float *out,
                   int out_len, char *err, int err_len) {
    auto *h = static_cast<Handle *>(handle);
    if (!h || !h->ctx) {
        set_err(err, err_len, "句柄无效");
        return -1;
    }
    if (!out || out_len < h->dim) {
        set_err(err, err_len, "输出缓冲区太小");
        return -1;
    }
    const char *body = text ? text : "";
    if (std::strlen(body) == 0) {
        set_err(err, err_len, "文本为空");
        return -1;
    }

    // EmbeddingGemma 系列是「带任务前缀」训练的：文档与查询必须用不同前缀，
    // 否则向量空间塌陷、无关文本余弦也会很高（表现为「语义搜索什么都能搜到」
    // 或「怎么都搜不到」）。llama.cpp 不会自动加，这里补上。
    std::string src;
    if (is_query) {
        src = "task: search result | query: ";
    } else {
        src = "title: none | text: ";
    }
    src += body;

    const int src_len = static_cast<int>(src.size());

    // 先问需要多少 token
    int32_t n = llama_tokenize(h->vocab, src.c_str(), src_len, nullptr, 0, true, false);
    if (n == INT32_MIN) {
        set_err(err, err_len, "分词溢出");
        return -1;
    }
    if (n < 0) {
        n = -n;  // 返回负值表示需要的缓冲区大小
    }
    if (n == 0) {
        set_err(err, err_len, "分词结果为空");
        return -1;
    }

    std::vector<llama_token> tokens(static_cast<size_t>(n));
    const int32_t got = llama_tokenize(h->vocab, src.c_str(), src_len, tokens.data(), n, true, false);
    if (got <= 0) {
        set_err(err, err_len, "分词失败");
        return -1;
    }
    tokens.resize(static_cast<size_t>(got));

    // 单条 decode 的 token 数不能超过 n_batch/n_ubatch，否则新版 llama.cpp 会在
    // encoder 断言里直接 abort。超长文本按 batch 上限截断（embedding 的常规做法：
    // 保留开头，开头对语义贡献最大）。
    if (h->n_batch > 0 && got > h->n_batch) {
        tokens.resize(static_cast<size_t>(h->n_batch));
    }

    llama_batch batch = llama_batch_get_one(tokens.data(), static_cast<int32_t>(tokens.size()));
    const int32_t rc = llama_decode(h->ctx, batch);
    if (rc != 0) {
        char buf[160];
        std::snprintf(buf, sizeof(buf), "llama_decode 返回 %d（上下文长度不足或内存不足）", rc);
        set_err(err, err_len, buf);
        return -1;
    }

    float *emb = llama_get_embeddings_seq(h->ctx, 0);
    if (!emb) {
        emb = llama_get_embeddings(h->ctx);
    }
    if (!emb) {
        set_err(err, err_len, "模型没有返回 embedding（可能缺少 pooling 配置）");
        return -1;
    }

    // L2 归一化，方便后续直接用点积当余弦
    double sum = 0.0;
    for (int i = 0; i < h->dim; ++i) {
        sum += static_cast<double>(emb[i]) * static_cast<double>(emb[i]);
    }
    const double norm = std::sqrt(sum);
    const float inv = norm > 1e-12 ? static_cast<float>(1.0 / norm) : 0.0f;
    for (int i = 0; i < h->dim; ++i) {
        out[i] = emb[i] * inv;
    }

    // 清掉上一轮的 KV/state，避免下一次 decode 累积
    llama_memory_t mem = llama_get_memory(h->ctx);
    if (mem) {
        llama_memory_clear(mem, true);
    }

    return h->dim;
}

// Encode several independent texts in one llama.cpp batch. Every text gets a
// separate sequence id, so the embedding context is shared without loading a
// second model or serialising GPU work through repeated single-token calls.
int ds_llama_embed_batch(void *handle, const char **texts, int count,
                         float *out, int out_stride, char *err, int err_len) {
    auto *h = static_cast<Handle *>(handle);
    if (!h || !h->ctx || !h->vocab) {
        set_err(err, err_len, "handle invalid");
        return -1;
    }
    if (!texts || count <= 0 || count > static_cast<int>(h->max_sequences) || !out || out_stride < h->dim) {
        set_err(err, err_len, "invalid batch arguments");
        return -1;
    }

    std::vector<std::vector<llama_token>> token_lists;
    token_lists.reserve(static_cast<size_t>(count));
    size_t total_tokens = 0;
    for (int i = 0; i < count; ++i) {
        const char *body = texts[i] ? texts[i] : "";
        if (!*body) {
            set_err(err, err_len, "text is empty");
            return -1;
        }
        std::string src = "title: none | text: ";
        src += body;
        int32_t n = llama_tokenize(h->vocab, src.c_str(), static_cast<int32_t>(src.size()),
                                   nullptr, 0, true, false);
        if (n == INT32_MIN) {
            set_err(err, err_len, "tokenization overflow");
            return -1;
        }
        if (n < 0) n = -n;
        if (n <= 0) {
            set_err(err, err_len, "tokenization returned no tokens");
            return -1;
        }
        std::vector<llama_token> tokens(static_cast<size_t>(n));
        const int32_t got = llama_tokenize(h->vocab, src.c_str(), static_cast<int32_t>(src.size()),
                                           tokens.data(), n, true, false);
        if (got <= 0) {
            set_err(err, err_len, "tokenization failed");
            return -1;
        }
        tokens.resize(static_cast<size_t>(got));
        if (h->n_batch > 0 && tokens.size() > static_cast<size_t>(h->n_batch)) {
            set_err(err, err_len, "one text exceeds the model batch context");
            return -1;
        }
        total_tokens += tokens.size();
        token_lists.push_back(std::move(tokens));
    }
    if (h->n_batch > 0 && total_tokens > static_cast<size_t>(h->n_batch)) {
        set_err(err, err_len, "batch exceeds the model context");
        return -1;
    }

    llama_batch batch = llama_batch_init(static_cast<int32_t>(total_tokens), 0, count);
    if (!batch.token || !batch.pos || !batch.n_seq_id || !batch.seq_id || !batch.logits) {
        llama_batch_free(batch);
        set_err(err, err_len, "failed to allocate llama batch");
        return -1;
    }
    batch.n_tokens = static_cast<int32_t>(total_tokens);
    size_t cursor = 0;
    for (int i = 0; i < count; ++i) {
        const auto &tokens = token_lists[static_cast<size_t>(i)];
        for (size_t pos = 0; pos < tokens.size(); ++pos, ++cursor) {
            batch.token[cursor] = tokens[pos];
            batch.pos[cursor] = static_cast<llama_pos>(pos);
            batch.n_seq_id[cursor] = 1;
            // llama_batch_init owns each seq_id array and llama_batch_free
            // releases it. Write into that allocation instead of replacing
            // the pointer with a temporary vector address.
            batch.seq_id[cursor][0] = static_cast<llama_seq_id>(i);
            batch.logits[cursor] = pos + 1 == tokens.size() ? 1 : 0;
        }
    }

    const auto clear_context = [h]() {
        llama_memory_t mem = llama_get_memory(h->ctx);
        if (mem) llama_memory_clear(mem, true);
    };
    const int32_t rc = llama_decode(h->ctx, batch);
    if (rc != 0) {
        llama_batch_free(batch);
        clear_context();
        char buf[160];
        std::snprintf(buf, sizeof(buf), "llama_decode returned %d", rc);
        set_err(err, err_len, buf);
        return -1;
    }
    for (int i = 0; i < count; ++i) {
        float *emb = llama_get_embeddings_seq(h->ctx, static_cast<llama_seq_id>(i));
        if (!emb) {
            llama_batch_free(batch);
            clear_context();
            set_err(err, err_len, "model returned no sequence embedding");
            return -1;
        }
        double sum = 0.0;
        for (int j = 0; j < h->dim; ++j) {
            sum += static_cast<double>(emb[j]) * static_cast<double>(emb[j]);
        }
        const double norm = std::sqrt(sum);
        const float inv = norm > 1e-12 ? static_cast<float>(1.0 / norm) : 0.0f;
        for (int j = 0; j < h->dim; ++j) {
            out[static_cast<size_t>(i) * static_cast<size_t>(out_stride) + static_cast<size_t>(j)] = emb[j] * inv;
        }
    }
    llama_batch_free(batch);
    clear_context();
    return count;
}

// ---------------------------------------------------------------------------
// 后端能力枚举（REQ-093 / REQ-096 / REQ-098 / REQ-100 / REQ-101）
//
// 需求表是按 PyTorch 写的（torch.cuda.is_available 之类）。本项目没有
// PyTorch，等价物就是「这个 exe 里到底编进了哪些 GGML 后端」—— 因为后端是
// 编译期决定的，运行时装不了，所以「检测」只需要问已链接进来的后端注册表。
// ---------------------------------------------------------------------------

int ds_llama_backend_count() {
    ensure_backend_initialized();
    return static_cast<int>(ggml_backend_reg_count());
}

int ds_llama_backend_info(int wanted, char *name, int name_len, char *features,
                          int features_len, int *device_count, int *has_accelerator) {
    ensure_backend_initialized();
    if (wanted < 0 || !name || name_len <= 0) {
        return -1;
    }
    if (static_cast<size_t>(wanted) >= ggml_backend_reg_count()) {
        return -1;
    }
    const auto reg = ggml_backend_reg_get(static_cast<size_t>(wanted));
    if (!reg) {
        return -1;
    }
    const char *reg_name = ggml_backend_reg_name(reg);
    std::snprintf(name, static_cast<size_t>(name_len), "%s", reg_name ? reg_name : "");

    // 后端的编译期特性（例如 Vulkan 的 device count、CUDA 的 compute cap）。
    // 这是判断「后端是否真的可用」与「它是加速器还是纯 CPU」的正路。
    std::string collected;
    if (features && features_len > 0) {
        const auto get_features = reinterpret_cast<ggml_backend_get_features_t>(
            ggml_backend_reg_get_proc_address(reg, "ggml_backend_get_features"));
        if (get_features) {
            for (const ggml_backend_feature *f = get_features(reg); f && f->name; ++f) {
                if (f->name[0] == '\0') {
                    break;
                }
                if (!collected.empty()) {
                    collected += ';';
                }
                collected += f->name;
                collected += '=';
                collected += f->value ? f->value : "";
            }
        }
    }

    // 加速器判定：该后端至少注册了一个 GPU / IGPU 设备。
    int accel = 0;
    const size_t devs = ggml_backend_reg_dev_count(reg);
    for (size_t i = 0; i < devs; ++i) {
        const auto dev = ggml_backend_reg_dev_get(reg, i);
        if (!dev) {
            continue;
        }
        const auto t = ggml_backend_dev_type(dev);
        if (t == GGML_BACKEND_DEVICE_TYPE_GPU || t == GGML_BACKEND_DEVICE_TYPE_IGPU) {
            accel = 1;
            break;
        }
    }
    if (device_count) {
        *device_count = static_cast<int>(devs);
    }
    if (has_accelerator) {
        *has_accelerator = accel;
    }
    if (features && features_len > 0) {
        std::snprintf(features, static_cast<size_t>(features_len), "%s", collected.c_str());
    }
    return 0;
}

// 按设备 id 探测显存分档（REQ-097）。返回 0..3，-1 表示设备不存在。
//
// 分档规则直接写在 C 侧，是因为 llama.cpp 的显存配额就是在这里被消费的，
// Rust 侧再算一遍容易和「实际下放策略」漂移。
int ds_llama_device_tier(const char *device_key_in) {
    ensure_backend_initialized();
    if (!device_key_in || !*device_key_in) {
        return -1;
    }
    for (size_t i = 0; i < ggml_backend_dev_count(); ++i) {
        const auto dev = ggml_backend_dev_get(i);
        const auto t = ggml_backend_dev_type(dev);
        if (t != GGML_BACKEND_DEVICE_TYPE_GPU && t != GGML_BACKEND_DEVICE_TYPE_IGPU) {
            continue;
        }
        if (device_key(dev) != device_key_in) {
            continue;
        }
        ggml_backend_dev_props props{};
        ggml_backend_dev_get_props(dev, &props);
        const size_t total = props.memory_total;
        if (total == 0) {
            // 未知显存：按集显处理，保守给 Tier 2。
            return 2;
        }
        constexpr size_t gib = 1024ull * 1024ull * 1024ull;
        if (total >= 8ull * gib) {
            return 0;
        }
        if (total >= 4ull * gib) {
            return 1;
        }
        if (total >= 1ull * gib) {
            return 2;
        }
        return 3;
    }
    return -1;
}

void ds_llama_free(void *handle) {
    auto *h = static_cast<Handle *>(handle);
    if (!h) {
        return;
    }
    if (h->ctx) {
        llama_free(h->ctx);
    }
    if (h->mtmd) {
        mtmd_free(h->mtmd);
    }
    release_shared_model(h->shared);
    delete h;
}

}  // extern "C"
