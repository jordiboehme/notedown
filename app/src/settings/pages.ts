// The pages of the settings window, in sidebar order and groups (decisions, "Panel and
// settings"). Each has a white line icon on a small coloured square, like Roger's panes; the
// colours are the macOS system colours Roger uses.

export type PageId =
  | "general"
  | "notifications"
  | "supernote"
  | "agents"
  | "ocr"
  | "server"
  | "account"
  | "setup"
  | "about";

export interface Page {
  id: PageId;
  title: string;
  /** The heading of its sidebar group; null for the groups without one. */
  group: string | null;
  /** The background of its icon. */
  color: string;
  /** SVG path data on a 24 x 24 grid, drawn as white strokes. */
  icon: string;
}

const GRAY = "#8e8e93";

export const PAGES: Page[] = [
  {
    id: "general",
    title: "General",
    group: null,
    color: GRAY,
    icon: "M12 8.5a3.5 3.5 0 1 0 0 7a3.5 3.5 0 1 0 0-7z M12 2.5v3 M12 18.5v3 M2.5 12h3 M18.5 12h3 M5.3 5.3l2.1 2.1 M16.6 16.6l2.1 2.1 M5.3 18.7l2.1-2.1 M16.6 7.4l2.1-2.1",
  },
  {
    id: "notifications",
    title: "Notifications",
    group: null,
    color: "#ff3b30",
    icon: "M6 16.5V11a6 6 0 0 1 12 0v5.5l1.5 1.5h-15z M10 20.5a2 2 0 0 0 4 0",
  },
  {
    id: "supernote",
    title: "Supernote",
    group: "Your notes",
    color: "#ff9500",
    icon: "M7 3h10a2 2 0 0 1 2 2v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2z M9 8h6 M9 12h6 M9 16h3",
  },
  {
    id: "agents",
    title: "Agents",
    group: "Your notes",
    color: "#30b0c7",
    icon: "M5 18a2 2 0 1 0 4 0a2 2 0 1 0-4 0z M15 18a2 2 0 1 0 4 0a2 2 0 1 0-4 0z M10 6a2 2 0 1 0 4 0a2 2 0 1 0-4 0z M8 16.3l3-8.4 M16 16.3l-3-8.4 M9 18h6",
  },
  {
    id: "ocr",
    title: "OCR",
    group: "Your notes",
    color: "#af52de",
    icon: "M4 8.5V5a1 1 0 0 1 1-1h3.5 M15.5 4H19a1 1 0 0 1 1 1v3.5 M20 15.5V19a1 1 0 0 1-1 1h-3.5 M8.5 20H5a1 1 0 0 1-1-1v-3.5 M8 10h8 M8 14h5",
  },
  {
    id: "server",
    title: "Server",
    group: "Connection",
    color: "#34c759",
    icon: "M4 4.5h16v6H4z M4 13.5h16v6H4z M7.5 7.5h.01 M7.5 16.5h.01",
  },
  {
    id: "account",
    title: "Account",
    group: "Connection",
    color: "#007aff",
    icon: "M12 12a4 4 0 1 0 0-8a4 4 0 1 0 0 8z M4.5 20.5a7.5 7.5 0 0 1 15 0",
  },
  {
    id: "setup",
    title: "Setup",
    group: null,
    color: "#5856d6",
    icon: "M4 7l2 2l3.5-3.5 M4 15l2 2l3.5-3.5 M13 7.5h7 M13 15.5h7",
  },
  {
    id: "about",
    title: "About",
    group: null,
    color: GRAY,
    icon: "M12 3a9 9 0 1 0 0 18a9 9 0 1 0 0-18z M12 11v6 M12 7.5v.01",
  },
];

/** The sidebar blocks, top to bottom; the first and the last have no heading. */
export const GROUPS: { title: string | null; pages: Page[] }[] = [
  { title: null, pages: PAGES.slice(0, 2) },
  { title: "Your notes", pages: PAGES.slice(2, 5) },
  { title: "Connection", pages: PAGES.slice(5, 7) },
  { title: null, pages: PAGES.slice(7) },
];

export function isPageId(id: unknown): id is PageId {
  return typeof id === "string" && PAGES.some((p) => p.id === id);
}

export function pageById(id: PageId): Page {
  return PAGES.find((p) => p.id === id) ?? PAGES[0];
}
