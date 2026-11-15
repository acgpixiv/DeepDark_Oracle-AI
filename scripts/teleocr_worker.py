import contextlib
import json
import os
import sys
import traceback


def emit(payload):
    sys.stdout.write(json.dumps(payload, ensure_ascii=False) + "\n")
    sys.stdout.flush()


def extract_page_text(blocks):
    parts = []
    for block in blocks or []:
        value = getattr(block, "content", "")
        if isinstance(value, str) and value.strip():
            parts.append(value.strip())
    return "\n".join(parts)


def extract_batch_text(result, count):
    """Normalize TeleOCR's batch result to one text value per input image.

    TeleOCR releases return a list of block lists for a batch.  A few older
    builds return a single block list when the batch has one image, so keep a
    conservative fallback shape here and let the caller retry individually
    if the result cannot be mapped one-to-one.
    """
    if not isinstance(result, list) or len(result) != count:
        return None
    return [extract_page_text(blocks) for blocks in result]


def main():
    model_dir = os.path.abspath(sys.argv[1])
    code_dir = os.path.abspath(sys.argv[2]) if len(sys.argv) > 2 and sys.argv[2] else ""
    if code_dir and code_dir not in sys.path:
        sys.path.insert(0, code_dir)

    try:
        with contextlib.redirect_stdout(sys.stderr):
            import fitz
            from PIL import Image
            from TeleOCR.vlm_utils.TeleOCR_model import TeleOCRMODEL_SERVICE

            predictor = TeleOCRMODEL_SERVICE.get_model("transformers", model_dir, None)
    except Exception as exc:
        emit({"ok": False, "error": "TeleOCR model load failed: " + str(exc), "trace": traceback.format_exc()})
        return 2

    emit({"ok": True, "ready": True})
    for line in sys.stdin:
        try:
            request = json.loads(line)
            if request.get("op") == "image":
                image_path = os.path.abspath(request["image_path"])
                image = Image.open(image_path).convert("RGB")
                with contextlib.redirect_stdout(sys.stderr):
                    result = predictor.batch_two_step_extract(images=[image])
                blocks = result[0] if result else []
                emit({"ok": True, "text": extract_page_text(blocks)})
                continue

            pdf_path = os.path.abspath(request["pdf_path"])
            page_numbers = request["pages"]
            pages = []
            errors = []
            with fitz.open(pdf_path) as document:
                images = []
                valid_page_numbers = []
                for page_number in page_numbers:
                    try:
                        index = int(page_number) - 1
                        if index < 0 or index >= document.page_count:
                            raise ValueError("page is outside PDF range: " + str(page_number))
                        pixmap = document[index].get_pixmap(matrix=fitz.Matrix(2, 2), alpha=False)
                        image = Image.frombytes("RGB", (pixmap.width, pixmap.height), pixmap.samples)
                        images.append(image)
                        valid_page_numbers.append(page_number)
                    except Exception as exc:
                        errors.append("page " + str(page_number) + ": " + str(exc))
                if images:
                    try:
                        with contextlib.redirect_stdout(sys.stderr):
                            result = predictor.batch_two_step_extract(images=images)
                        texts = extract_batch_text(result, len(images))
                        if texts is None:
                            raise RuntimeError("TeleOCR returned an unexpected batch result")
                        pages.extend(
                            {"page": page_number, "text": text}
                            for page_number, text in zip(valid_page_numbers, texts)
                        )
                    except Exception as batch_exc:
                        # Keep compatibility with TeleOCR versions that only
                        # implement a single-image batch call.  A bad batch
                        # response must not discard pages that can still be
                        # recognized individually.
                        for page_number, image in zip(valid_page_numbers, images):
                            try:
                                with contextlib.redirect_stdout(sys.stderr):
                                    result = predictor.batch_two_step_extract(images=[image])
                                blocks = result[0] if result else []
                                pages.append({"page": page_number, "text": extract_page_text(blocks)})
                            except Exception as exc:
                                errors.append("page " + str(page_number) + ": " + str(exc))
                        if not pages and valid_page_numbers:
                            errors.append("batch: " + str(batch_exc))
            emit({"ok": True, "pages": pages, "errors": errors})
        except Exception as exc:
            emit({"ok": False, "error": "TeleOCR request failed: " + str(exc), "trace": traceback.format_exc()})


if __name__ == "__main__":
    raise SystemExit(main())
