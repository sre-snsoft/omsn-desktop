import { useEffect, useMemo, useState } from 'react';

/** Measured height of one task row (title + meta line + borders). */
const ROW_HEIGHT_PX = 55;
/** Titlebar (33) + pager (35) + list padding. */
const CHROME_PX = 76;
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
