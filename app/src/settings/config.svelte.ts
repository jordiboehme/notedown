import { getConfig, failure, type AppConfig } from "./api";

/** The app config as the settings window knows it; null until it is read. */
export const config = $state<{ current: AppConfig | null; problem: string | null }>({
  current: null,
  problem: null,
});

export async function loadConfig(): Promise<void> {
  try {
    config.current = await getConfig();
    config.problem = null;
  } catch (error) {
    config.problem = failure(error);
  }
}
