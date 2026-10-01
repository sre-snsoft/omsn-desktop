import { useEffect, useState } from 'react';
import { getVersion } from '@tauri-apps/api/app';

/** One tick a second.
 *
 *  A minute would be enough for the clock itself, but the pager's "Xm ago"
 *  label only refreshes when something else re-renders. A 1 s tick makes it
 *  live, which is the whole reason the clock is worth having. */
const TICK_MS = 1_000;

/** Wall-clock time, re-read every second. */
export function useNow(): Date {
  const [now, setNow] = useState(() => new Date());

  useEffect(() => {
    const timer = window.setInterval(() => setNow(new Date()), TICK_MS);
    return () => window.clearInterval(timer);
  }, []);

  return now;
}

/**
 * The running version, asked of the bundle at runtime.
 *
 * The number lives in three files (package.json, Cargo.toml,
 * tauri.conf.json). Reading it from the Tauri API means the footer cannot
 * disagree with the version actually installed, whichever of the three
 * someone forgot to bump. Already permitted: `core:default` grants
 * `core:app:allow-version`, so no capability change.
 */
export function useAppVersion(): string | null {
  const [version, setVersion] = useState<string | null>(null);

  useEffect(() => {
    let alive = true;
    getVersion()
      .then((v) => {
        if (alive) setVersion(v);
      })
      .catch(() => {
        // Deliberately not surfaced. The version line is decoration; a
        // failure here must not put an error notice over someone's tasks.
        // `null` renders as nothing at all.
      });
    return () => {
      alive = false;
    };
  }, []);

  return version;
}
