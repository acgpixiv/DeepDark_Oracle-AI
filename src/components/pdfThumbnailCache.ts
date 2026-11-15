/** Small process-wide LRU for PDF page thumbnails. Keeping this outside Vue
 * component setup lets search-result cards reuse rendered pages when filters
 * unmount and recreate the cards. */
type CachedThumbnail = { value: string; bytes: number };

const pageCache = new Map<string, CachedThumbnail>();
const PAGE_CACHE_LIMIT = 512;
// A count-only cap lets a few large, high-DPI pages retain hundreds of MB of
// base64 strings. Keep the cross-filter cache useful without allowing it to
// compete with PDF.js and the search result grid for memory.
const PAGE_CACHE_BYTE_LIMIT = 48 * 1024 * 1024;
let pageCacheBytes = 0;

function dataUrlBytes(value: string): number {
  const comma = value.indexOf(",");
  const payload = comma >= 0 ? value.slice(comma + 1) : value;
  // JPEG data URLs are base64. The fallback is intentionally conservative for
  // any future non-base64 cache value.
  return Math.ceil(payload.length * (payload.endsWith("==") ? 0.75 : payload.endsWith("=") ? 0.75 : 0.75));
}

export function pdfThumbnailCacheKey(
  fileId: number,
  path: string,
  mtime: number | undefined,
  page: number | undefined,
): string {
  const pageNo = Number.isFinite(page) ? Math.max(1, Math.floor(page || 1)) : 1;
  return `${fileId}:${path}:m${mtime ?? 0}:p${pageNo}`;
}

export function getPdfThumbnail(key: string): string | undefined {
  const entry = pageCache.get(key);
  if (entry) {
    // Refresh LRU position on reuse.
    pageCache.delete(key);
    pageCache.set(key, entry);
    return entry.value;
  }
  return undefined;
}

export function rememberPdfThumbnail(key: string, value: string): void {
  const previous = pageCache.get(key);
  if (previous) pageCacheBytes -= previous.bytes;
  const entry = { value, bytes: dataUrlBytes(value) };
  pageCache.set(key, entry);
  pageCacheBytes += entry.bytes;
  while (pageCache.size > PAGE_CACHE_LIMIT || pageCacheBytes > PAGE_CACHE_BYTE_LIMIT) {
    const oldest = pageCache.entries().next().value as [string, CachedThumbnail] | undefined;
    if (!oldest) break;
    pageCache.delete(oldest[0]);
    pageCacheBytes -= oldest[1].bytes;
  }
}
