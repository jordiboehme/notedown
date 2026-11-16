import { isPageId, type PageId } from "../settings/pages";

/** The key of the last settings page, as Roger's `settingsLastPane`. */
export const LAST_PAGE_KEY = "settingsLastPage";

/** The part of `Storage` the store needs; tests pass their own. */
export interface PageStorage {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
}

/**
 * Remembers the last settings page. An unknown page, or storage that cannot be read, gives
 * General; a failed save is ignored.
 */
export function createLastPage(storage: PageStorage) {
  return {
    load(): PageId {
      try {
        const stored = storage.getItem(LAST_PAGE_KEY);
        return isPageId(stored) ? stored : "general";
      } catch {
        return "general";
      }
    },
    save(id: PageId): void {
      try {
        storage.setItem(LAST_PAGE_KEY, id);
      } catch {
        // Nothing to do: the page is only not remembered.
      }
    },
  };
}

/** The page to open: the one Rust asks for (Set up in the panel), else the last one. */
export function firstPage(requested: string | null | undefined, last: PageId): PageId {
  return isPageId(requested) ? requested : last;
}
