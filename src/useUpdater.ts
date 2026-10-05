import { useCallback, useState } from 'react';
import { check } from '@tauri-apps/plugin-updater';
import { relaunch } from '@tauri-apps/plugin-process';

/**
 * Update state machine.
 *
 * Deliberately manual: checking runs when the panel opens or the user asks,
 * never on a timer. The app already polls Lark every 20s while focused, and a
 * background update poll for 14 people buys nothing.
 */
export type UpdateState =
  | { phase: 'idle' }
  | { phase: 'checking' }
  | { phase: 'current' }
  | { phase: 'available'; version: string; notes: string }
  | { phase: 'downloading'; percent: number | null }
  | { phase: 'ready' }
  | { phase: 'failed'; message: string };

/** Keep a long release note from stretching the panel. */
const MAX_NOTES = 300;

export function useUpdater() {
  const [state, setState] = useState<UpdateState>({ phase: 'idle' });

  const checkNow = useCallback(async () => {
    setState({ phase: 'checking' });
    try {
      const update = await check();
      if (!update) {
        setState({ phase: 'current' });
        return;
      }
      setState({
        phase: 'available',
        version: update.version,
        notes: (update.body ?? '').slice(0, MAX_NOTES),
      });
    } catch (err) {
      // No release published yet is the common case, not a crash.
      setState({ phase: 'failed', message: describe(err) });
    }
  }, []);

  const installNow = useCallback(async () => {
    setState({ phase: 'downloading', percent: null });
    try {
      const update = await check();
      if (!update) {
        setState({ phase: 'current' });
        return;
      }

      let total = 0;
      let seen = 0;
      await update.downloadAndInstall((event) => {
        if (event.event === 'Started') {
          total = event.data.contentLength ?? 0;
          setState({ phase: 'downloading', percent: total ? 0 : null });
        } else if (event.event === 'Progress') {
          seen += event.data.chunkLength ?? 0;
          setState({
            phase: 'downloading',
            percent: total ? Math.min(100, Math.round((seen / total) * 100)) : null,
          });
        } else if (event.event === 'Finished') {
          setState({ phase: 'ready' });
        }
      });

      setState({ phase: 'ready' });
      // Replacing a running bundle means the old code is already gone; restart
      // rather than leave the user on a version that no longer exists on disk.
      await relaunch();
    } catch (err) {
      setState({ phase: 'failed', message: describe(err) });
    }
  }, []);

  const reset = useCallback(() => setState({ phase: 'idle' }), []);

  return { state, checkNow, installNow, reset };
}

/** A readable sentence, never a raw object. */
function describe(err: unknown): string {
  const raw = err instanceof Error ? err.message : String(err);
  if (/404|not found|no releases?/i.test(raw)) {
    return 'No releases published yet.';
  }
  if (/network|dns|connect|timed? out/i.test(raw)) {
    return 'Could not reach GitHub. Check your connection.';
  }
  if (/signature|pubkey|verif/i.test(raw)) {
    // Worth saying plainly: a bad signature means do not install.
    return 'Update signature did not verify — refusing to install.';
  }
  return raw.slice(0, 160);
}
