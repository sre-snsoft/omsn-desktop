import { useCallback, useEffect, useRef, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import type { Pin, Snapshot, Task, UiError, Viewer } from './types';
import { formatClock, initials, pinTask, sortTasks } from './types';
import { TaskRow } from './TaskRow';
import { useAppVersion, useNow } from './useChrome';
import { usePagination } from './usePagination';
import { Settings } from './Settings';
import './App.css';

/** How often to re-read the Base, while the window has focus. */
const POLL_INTERVAL_MS = 20_000;

/** Rust emits this the moment a write settles, so the in-flight marker clears
 *  on the answer rather than on the next tick of a timer. `core:app:default`
 *  already grants `allow-register-listener`; no capability change. */
const WRITE_SETTLED_EVENT = 'omsn://write-settled';

/** Backstop for an event that never arrives. Deliberately not a backoff: the
 *  read it performs is a pure in-memory call with no network behind it, so
 *  making later feedback slower would buy nothing and cost the user. */
const SETTLE_BACKSTOP_MS = 1_000;

/** Two network refreshes closer together than this are the same intent — a
 *  window being alt-tabbed, or a finger on the refresh button. The widget is
 *  always on top, so focus flaps constantly. */
const MIN_REFRESH_GAP_MS = 2_000;

/** Stop chasing a write that has not settled in this long. Past that it is
 *  the store's 60 s overlay timeout's problem, not the UI's. */
const SETTLE_WINDOW_MS = 12_000;

function relativeTime(millis: number): string {
  const mins = Math.floor((Date.now() - millis) / 60_000);
  if (mins < 1) return 'just now';
  if (mins < 60) return `${mins}m ago`;
  const hours = Math.floor(mins / 60);
  return hours < 24 ? `${hours}h ago` : `${Math.floor(hours / 24)}d ago`;
}

export default function App() {
  const [viewer, setViewer] = useState<Viewer | null>(null);
  const [snapshot, setSnapshot] = useState<Snapshot | null>(null);
  const [error, setError] = useState<UiError | null>(null);
  const [busy, setBusy] = useState(false);
  const [toast, setToast] = useState<string | null>(null);
  const [draft, setDraft] = useState('');
  const [adding, setAdding] = useState(false);
  const [confirmDone, setConfirmDone] = useState<Task | null>(null);
  const [showSettings, setShowSettings] = useState(false);
  const [confirmDelete, setConfirmDelete] = useState<Task | null>(null);
  const [expandedId, setExpandedId] = useState<string | null>(null);
  const [pin, setPin] = useState<Pin | null>(null);
  // A rejected write is reported separately from `error`: a poll succeeding
  // clears `error`, and that must not erase the only notice the user ever got
  // that their change did not stick.
  const [writeError, setWriteError] = useState<string | null>(null);
  const [seenFailureSeq, setSeenFailureSeq] = useState(0);

  const now = useNow();
  const version = useAppVersion();

  // Completed work is history, not a to-do list. On Hold stays visible: it is
  // blocked, not finished, and hiding it would let it rot unseen.
  const tasks = pinTask(
    sortTasks((snapshot?.tasks ?? []).filter((t) => t.status !== 'Done')),
    pin
  );
  const pager = usePagination(tasks);

  // When the last network refresh was asked for, so three unthrottled
  // triggers cannot stack walks. A ref, not state: changing it must not
  // re-render, and `load` must not be rebuilt by it.
  const lastRefreshAt = useRef(0);

  const load = useCallback(async (refresh: boolean) => {
    if (refresh) {
      const at = Date.now();
      if (at - lastRefreshAt.current < MIN_REFRESH_GAP_MS) return;
      lastRefreshAt.current = at;
    }
    try {
      setSnapshot(await invoke<Snapshot>('list_my_tasks', { refresh }));
      setError(null);
    } catch (err) {
      setError(err as UiError);
    }
  }, []);

  /// Full browser consent. The only path that works for someone with no
  /// stored session — a new teammate, or an expired refresh token.
  const authorize = useCallback(async () => {
    setBusy(true);
    setError(null);
    setToast('Opening your browser...');
    try {
      setViewer(await invoke<Viewer>('authorize'));
      await load(true);
      setToast('Signed in');
    } catch (err) {
      setError(err as UiError);
    } finally {
      setBusy(false);
    }
  }, [load]);

  const signIn = useCallback(async () => {
    setBusy(true);
    // Clear first so a repeated failure still reads as a fresh attempt rather
    // than an unchanged screen.
    setError(null);
    try {
      setViewer(await invoke<Viewer>('sign_in'));
      await load(true);
      setToast('Signed in');
    } catch (err) {
      setError(err as UiError);
    } finally {
      setBusy(false);
    }
  }, [load]);

  useEffect(() => {
    void signIn();
  }, [signIn]);

  // Poll only while focused, and refresh the moment focus returns.
  useEffect(() => {
    if (!viewer) return;
    const tick = () => {
      if (document.hasFocus()) void load(true);
    };
    const timer = window.setInterval(tick, POLL_INTERVAL_MS);
    window.addEventListener('focus', tick);
    return () => {
      window.clearInterval(timer);
      window.removeEventListener('focus', tick);
    };
  }, [viewer, load]);

  // Rust tells us the instant a write settles. `refresh: false` asks only what
  // Rust already believes, so this costs no network at all.
  useEffect(() => {
    // Caught here rather than in the cleanup: a listener that cannot be
    // registered is not worth an error notice over someone's tasks, and the
    // backstop below covers it.
    const listening = listen(WRITE_SETTLED_EVENT, () => {
      void load(false);
    }).catch(() => null);
    return () => {
      void listening.then((stop) => stop?.());
    };
  }, [load]);

  // A missed or duplicated event must not strand a row dimmed forever, so a
  // slow read keeps running while anything is still in flight.
  const settling = (snapshot?.pending_ids.length ?? 0) > 0;
  useEffect(() => {
    if (!settling) return;
    const giveUpAt = Date.now() + SETTLE_WINDOW_MS;
    const timer = window.setInterval(() => {
      if (Date.now() > giveUpAt) {
        window.clearInterval(timer);
        return;
      }
      void load(false);
    }, SETTLE_BACKSTOP_MS);
    return () => window.clearInterval(timer);
  }, [settling, load]);

  // Escape dismisses the delete confirm, like clicking the backdrop. The
  // destructive path must always have a way out that needs no aim.
  useEffect(() => {
    if (!confirmDelete) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') setConfirmDelete(null);
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [confirmDelete]);

  // A write the Base refused. The command returned before the answer arrived,
  // so this is the only place the user can be told — never skip it.
  useEffect(() => {
    const fresh = (snapshot?.write_failures ?? []).filter((f) => f.seq > seenFailureSeq);
    if (fresh.length === 0) return;
    const newest = fresh[fresh.length - 1];
    setSeenFailureSeq(newest.seq);
    setWriteError(newest.message);
  }, [snapshot, seenFailureSeq]);

  // A poll has re-read the server, so the list order it returns is the truth.
  useEffect(() => {
    if (pin && snapshot && snapshot.fetched_at_millis !== pin.fetchedAt) setPin(null);
  }, [pin, snapshot]);

  // Page height is predictable only if at most one row is expanded, and an
  // expansion carried across a page change would be invisible.
  useEffect(() => {
    setExpandedId(null);
  }, [pager.page]);

  useEffect(() => {
    if (!toast) return;
    const timer = window.setTimeout(() => setToast(null), 2600);
    return () => window.clearTimeout(timer);
  }, [toast]);

  /// Hold a row in the slot it was in: any write moves `Modified`, and the
  /// sort orders within a status group by `Modified` ascending, so the row
  /// the user just touched would otherwise jump to the bottom of the list.
  const pinRow = (task: Task) => {
    const index = tasks.findIndex((t) => t.record_id === task.record_id);
    if (index < 0) return;
    setPin({
      recordId: task.record_id,
      index,
      fetchedAt: snapshot?.fetched_at_millis ?? 0,
    });
  };

  const setStatus = async (task: Task, status: string) => {
    setWriteError(null);
    // Done is exempt: it leaves the active list entirely, so there is nothing
    // to hold in place.
    if (status !== 'Done') pinRow(task);
    try {
      // Returns as soon as Rust has overlaid the change; the PUT continues in
      // the background and settles itself.
      setSnapshot(
        await invoke<Snapshot>('update_task', {
          recordId: task.record_id,
          patch: { status },
        })
      );
      setToast(status === 'Done' ? `Done: ${task.title}` : `-> ${status}`);
    } catch (err) {
      setPin(null);
      setError(err as UiError);
    }
  };

  /// Fix a typo in a title. Only `title` travels, so a concurrent plugin edit
  /// to any other column on the same record is not clobbered.
  ///
  /// Resolves false when the write was refused, so the editor stays open on
  /// the text the user typed. A blank title is refused in Rust by
  /// `TaskPatch::validate`, and the reason is shown rather than swallowed.
  const renameTask = async (task: Task, title: string): Promise<boolean> => {
    setWriteError(null);
    const next = title.trim();
    if (next === task.title) return true;
    pinRow(task);
    try {
      setSnapshot(
        await invoke<Snapshot>('update_task', {
          recordId: task.record_id,
          patch: { title: next },
        })
      );
      setToast(`Renamed: ${next}`);
      return true;
    } catch (err) {
      setPin(null);
      setWriteError((err as UiError).message);
      return false;
    }
  };

  /// Irreversible, and confirmed before it gets here. Unlike a status change
  /// this waits for the Base: a row that vanished optimistically and then
  /// came back would be worse than a moment's delay.
  const removeTask = async (task: Task) => {
    setWriteError(null);
    try {
      setSnapshot(await invoke<Snapshot>('delete_task', { recordId: task.record_id }));
      setPin(null);
      setExpandedId(null);
      setToast(`Deleted: ${task.title}`);
    } catch (err) {
      setWriteError((err as UiError).message);
    }
  };

  const addTask = async () => {
    const title = draft.trim();
    if (!title || adding) return;
    setAdding(true);
    try {
      // New work starts in flight: you add it because you are doing it.
      setSnapshot(
        await invoke<Snapshot>('create_task', { patch: { title, status: 'In Progress' } })
      );
      setDraft('');
      setPin(null);
      pager.reset();
      setToast(`Added: ${title}`);
    } catch (err) {
      setError(err as UiError);
    } finally {
      setAdding(false);
    }
  };

  const pendingIds = new Set(snapshot?.pending_ids ?? []);
  const stamp = version ? `v${version}` : '';
  const who = initials(viewer?.display_name);

  return (
    <div className={`app ${showSettings ? 'app--behind-sheet' : ''}`}>
      <header className="titlebar" data-tauri-drag-region>
        <span className="titlebar__name" data-tauri-drag-region>
          OMSN
        </span>
        <span className="titlebar__clock" data-tauri-drag-region>
          {formatClock(now)}
        </span>
        <span className="titlebar__count" data-tauri-drag-region>
          {viewer ? `${tasks.length} ACTIVE` : ''}
        </span>
        <button
          className="pixel-btn pixel-btn--ghost"
          onClick={() => void load(true)}
          title="Refresh"
        >
          ↻
        </button>
        <button
          className="pixel-btn pixel-btn--ghost"
          onClick={() => setShowSettings(true)}
          title="Settings"
        >
          ⚙
        </button>
      </header>

      {error && (
        <div className="notice notice--error">
          <span>{error.message}</span>
          {error.kind === 'config' && (
            <span className="notice__hint">Check ~/.config/omsn/desktop.env</span>
          )}
          {error.needs_login && (
            <button className="pixel-btn" onClick={() => void authorize()} disabled={busy}>
              {busy ? 'WAIT...' : 'SIGN IN'}
            </button>
          )}
        </div>
      )}

      {writeError && (
        <div className="notice notice--error">
          <span>Change not saved. {writeError}</span>
          <button
            className="pixel-btn pixel-btn--ghost"
            title="Dismiss"
            onClick={() => setWriteError(null)}
          >
            ✕
          </button>
        </div>
      )}

      {snapshot?.stale && !error && (
        <div className="notice notice--warn">Offline — showing last known tasks.</div>
      )}

      {viewer && (
        <div className="add">
          <input
            value={draft}
            placeholder="new task..."
            maxLength={200}
            onChange={(e) => setDraft(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === 'Enter') void addTask();
            }}
          />
          <button
            className="pixel-btn"
            onClick={() => void addTask()}
            disabled={!draft.trim() || adding}
            title="Add as In Progress"
          >
            {adding ? '...' : '+'}
          </button>
        </div>
      )}

      <main className="list">
        {!viewer && busy && <p className="empty">CONNECTING...</p>}

        {!viewer && !busy && (
          <div className="welcome">
            <p className="welcome__title">OMSN DESKTOP</p>
            <p className="welcome__sub">
              Sign in with Lark to see
              <br />
              the tasks assigned to you.
            </p>
            <button className="pixel-btn" onClick={() => void authorize()} disabled={busy}>
              SIGN IN WITH LARK
            </button>
          </div>
        )}

        {viewer && tasks.length === 0 && (
          <p className="empty">
            NOTHING ASSIGNED
            <br />
            that&apos;s a real state, not a bug
          </p>
        )}

        <ul>
          {pager.visible.map((task) => (
            <TaskRow
              key={task.record_id}
              task={task}
              pending={pendingIds.has(task.record_id)}
              expanded={expandedId === task.record_id}
              onToggleExpand={(id) => setExpandedId((current) => (current === id ? null : id))}
              onSetStatus={(t, s) => void setStatus(t, s)}
              onAskDone={setConfirmDone}
              onRename={renameTask}
              onAskDelete={setConfirmDelete}
            />
          ))}
        </ul>
      </main>

      {confirmDone && (
        <div className="confirm" onClick={() => setConfirmDone(null)}>
          <div className="confirm__box" onClick={(e) => e.stopPropagation()}>
            <div className="confirm__title">MARK AS DONE?</div>
            <div className="confirm__task">{confirmDone.title}</div>
            <div className="confirm__row">
              <button className="pixel-btn pixel-btn--ghost" onClick={() => setConfirmDone(null)}>
                NO
              </button>
              <button
                className="pixel-btn pixel-btn--ok"
                onClick={() => {
                  const task = confirmDone;
                  setConfirmDone(null);
                  void setStatus(task, 'Done');
                }}
              >
                YES
              </button>
            </div>
          </div>
        </div>
      )}

      {confirmDelete && (
        <div className="confirm" onClick={() => setConfirmDelete(null)}>
          <div className="confirm__box" onClick={(e) => e.stopPropagation()}>
            <div className="confirm__title">DELETE FOREVER?</div>
            <div className="confirm__task">{confirmDelete.title}</div>
            {/* Worded as permanent on purpose: whether this Base keeps a
                recoverable trash has not been established, and the copy must
                not promise an undo nobody has verified exists. */}
            <div className="confirm__warn">
              This removes the record from the team Base. It cannot be undone from this app.
            </div>
            <div className="confirm__row">
              <button
                className="pixel-btn pixel-btn--ghost"
                autoFocus
                onClick={() => setConfirmDelete(null)}
              >
                CANCEL
              </button>
              {/* Reads DELETE, not YES: muscle memory from MARK AS DONE? must
                  not be able to destroy a record. */}
              <button
                className="pixel-btn pixel-btn--danger"
                onClick={() => {
                  const task = confirmDelete;
                  setConfirmDelete(null);
                  void removeTask(task);
                }}
              >
                DELETE
              </button>
            </div>
          </div>
        </div>
      )}

      {showSettings && (
        <Settings
          viewer={viewer}
          version={version}
          onClose={() => setShowSettings(false)}
        />
      )}

      {toast && <div className="toast">{toast}</div>}

      <footer className="pager">
        <button
          className="pixel-btn pixel-btn--ghost"
          onClick={pager.prev}
          disabled={!pager.hasPrev}
        >
          ‹
        </button>
        <span className="pager__label">
          {pager.pageCount > 1 ? `${pager.page + 1}/${pager.pageCount}` : ''}
        </span>
        <button
          className="pixel-btn pixel-btn--ghost"
          onClick={pager.next}
          disabled={!pager.hasNext}
        >
          ›
        </button>
        <span className="pager__version">{who ? `${stamp} · ${who}` : stamp}</span>
        <span className="pager__synced">
          {snapshot?.fetched_at_millis ? relativeTime(snapshot.fetched_at_millis) : ''}
        </span>
      </footer>
    </div>
  );
}
