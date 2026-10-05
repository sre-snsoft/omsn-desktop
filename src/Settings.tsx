import type { Viewer } from './types';
import { initials } from './types';
import { useUpdater } from './useUpdater';

/**
 * Settings, shown as an overlay over a blurred app.
 *
 * Only holds what there is actually something to say about: who is signed in,
 * and updates. An empty settings page is worse than no settings page.
 */
export function Settings({
  viewer,
  version,
  onClose,
}: {
  viewer: Viewer | null;
  version: string | null;
  onClose: () => void;
}) {
  const { state, checkNow, installNow } = useUpdater();

  return (
    <div className="sheet" onClick={onClose}>
      <div
        className="sheet__box"
        role="dialog"
        aria-label="Settings"
        onClick={(e) => e.stopPropagation()}
      >
        <header className="sheet__head">
          <span className="sheet__title">SETTINGS</span>
          <button className="pixel-btn pixel-btn--ghost" onClick={onClose} title="Close">
            ×
          </button>
        </header>

        <section className="card">
          <div className="card__title">SIGNED IN</div>
          {viewer ? (
            <div className="card__row">
              <span className="avatar">{initials(viewer.display_name)}</span>
              <span>{viewer.display_name}</span>
            </div>
          ) : (
            <div className="card__muted">Not signed in</div>
          )}
        </section>

        <section className="card">
          <div className="card__title">APP UPDATES</div>
          <div className="card__muted">
            Current version: {version ? `v${version}` : "unknown"}
          </div>

          <div className="card__status">
            {state.phase === 'idle' && <span className="card__muted">Not checked yet.</span>}
            {state.phase === 'checking' && <span>Checking…</span>}
            {state.phase === 'current' && <span className="ok">You&apos;re up to date.</span>}
            {state.phase === 'available' && (
              <span className="warn">v{state.version} is available.</span>
            )}
            {state.phase === 'downloading' && (
              <span>
                Downloading
                {state.percent === null ? '…' : ` ${state.percent}%`}
              </span>
            )}
            {state.phase === 'ready' && <span className="ok">Installed — restarting…</span>}
            {state.phase === 'failed' && <span className="bad">{state.message}</span>}
          </div>

          {state.phase === 'available' && state.notes && (
            <div className="card__notes">{state.notes}</div>
          )}

          <div className="card__actions">
            {state.phase === 'available' ? (
              <button className="pixel-btn pixel-btn--ok" onClick={() => void installNow()}>
                UPDATE &amp; RESTART
              </button>
            ) : (
              <button
                className="pixel-btn"
                onClick={() => void checkNow()}
                disabled={state.phase === 'checking' || state.phase === 'downloading'}
              >
                {state.phase === 'idle' ? 'CHECK' : 'CHECK AGAIN'}
              </button>
            )}
          </div>
        </section>
      </div>
    </div>
  );
}
