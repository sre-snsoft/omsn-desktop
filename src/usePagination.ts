import { useEffect, useMemo, useState } from 'react';

/** Measured height of one **collapsed** task row at the current type scale:
 *  padding 24 + title 18 + gap 6 + meta 15 + border 2.
 *
 *  An expanded row (R3) is taller, by as many lines as its title needs. Page
 *  size is deliberately not reduced to compensate: shrinking `perPage` while
 *  a row is open could push that very row onto the next page, which is the
 *  "the row I touched disappeared" failure R5 exists to remove. `.list` is
 *  `overflow-y: auto`, so the page scrolls instead of clipping, and
 *  `TaskRow` scrolls the row it just opened back into view. */
const ROW_HEIGHT_PX = 66;
/** Titlebar (44) + add bar (48) + pager (46) + list padding.
 *  The add bar was missing from this total, which overflowed the list. */
const CHROME_PX = 142;
const MIN_PER_PAGE = 3;

/**
 * Paginate a list, sizing pages to the window rather than a fixed count.
 *
 * The widget is freely resizable, so a fixed page size would either waste a
 * tall window or overflow a short one.
 */
export function usePagination<T>(items: T[]) {
  const [page, setPage] = useState(0);
  const [viewportHeight, setViewportHeight] = useState(window.innerHeight);

  useEffect(() => {
    const onResize = () => setViewportHeight(window.innerHeight);
    window.addEventListener('resize', onResize);
    return () => window.removeEventListener('resize', onResize);
  }, []);

  const perPage = Math.max(
    MIN_PER_PAGE,
    Math.floor((viewportHeight - CHROME_PX) / ROW_HEIGHT_PX)
  );
  const pageCount = Math.max(1, Math.ceil(items.length / perPage));

  // Resizing or a shrinking list can strand the user past the last page.
  useEffect(() => {
    if (page >= pageCount) setPage(pageCount - 1);
  }, [page, pageCount]);

  const safePage = Math.min(page, pageCount - 1);
  const visible = useMemo(
    () => items.slice(safePage * perPage, safePage * perPage + perPage),
    [items, safePage, perPage]
  );

  return {
    visible,
    page: safePage,
    pageCount,
    perPage,
    hasPrev: safePage > 0,
    hasNext: safePage < pageCount - 1,
    prev: () => setPage((p) => Math.max(0, p - 1)),
    next: () => setPage((p) => Math.min(pageCount - 1, p + 1)),
    reset: () => setPage(0),
  };
}
