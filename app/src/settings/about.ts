// The texts of the About page (decisions, "Panel and settings"); AboutPage.svelte renders them.

export interface AboutLink {
  label: string;
  url: string;
  /** The filled button; the other one is bordered. */
  strong: boolean;
}

export const ABOUT = {
  name: "Notedown",
  motto: "Written by hand. Read by machine.",
  links: [
    { label: "Support Me", url: "https://ko-fi.com/V7V31T6CL9", strong: true },
    { label: "GitHub", url: "https://github.com/jordiboehme/notedown", strong: false },
  ] as AboutLink[],
  footer: ["Powered by llama.cpp, Qwen and PaddlePaddle models", "Created with love by Jordi Böhme, AGPL-3.0"],
  version: (version: string) => `Version ${version}`,
};
