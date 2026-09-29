import { useCallback, useEffect, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import type { Snapshot, Task, UiError, Viewer } from './types';
import { STATUS_CLASS, STATUS_ICON, daysSince, isStale, sortTasks } from './types';
import { usePagination } from './usePagination';
import './App.css';

/** How often to re-read the Base, while the window has focus. */
const POLL_INTERVAL_MS = 20_000;

/** Clicking the marker moves work forward. Completing is deliberately absent:
 *  it removes the row, so it is confirmed rather than one click away. */
const ADVANCE: Record<string, string> = {
  Backlog: 'In Progress',
  'This Week': 'In Progress',
  'On Hold': 'In Progress',
};

function relativeTime(millis: number): string {
  const mins = Math.floor((Date.now() - millis) / 60_000);
  if (mins < 1) return 'just now';
  if (mins < 60) return `${mins}m ago`;
  const hours = Math.floor(mins / 60);
  return hours < 24 ? `${hours}h ago` : `${Math.floor(hours / 24)}d ago`;
}

function TaskRow({
  task,
  pending,
  onSetStatus,
  onAskDone,
}: {
  task: Task;
  pending: boolean;
  onSetStatus: (task: Task, status: string) => void;
  onAskDone: (task: Task) => void;
}) {
  const age = daysSince(task.modified ?? task.created);
  const stale = isStale(task);
  const inProgress = task.status === 'In Progress';

  return (
    <li className={`task ${pending ? 'task--pending' : ''}`}>
      <button
        className={`task__status task__status--${STATUS_CLASS[task.status] ?? 'backlog'}`}
        title={task.status}
        onClick={() => {
          const next = ADVANCE[task.status];
          if (next) onSetStatus(task, next);
        }}
      >
        {STATUS_ICON[task.status] ?? '○'}
      </button>

      <div className="task__body">
        <span className="task__title" title={task.title}>
          {task.title}
        </span>
        <span className="task__meta">
          {task.priority && (
            <span className={`chip chip--${task.priority.slice(0, 2).toLowerCase()}`}>
              {task.priority.slice(0, 2)}
            </span>
          )}
          {age !== null && (
            <span className={stale ? 'age age--stale' : 'age'}>
              {age}d{stale ? ' STALE' : ''}
            </span>
          )}
        </span>
      </div>

      <div className="task__actions">
        {inProgress ? (
          <>
            <button
              className="pixel-btn pixel-btn--ghost"
              title="Back to Backlog"
              onClick={() => onSetStatus(task, 'Backlog')}
            >
              ←
            </button>
            <button
              className="pixel-btn pixel-btn--ok"
              title="Mark done"
              onClick={() => onAskDone(task)}
            >
              ✓
            </button>
          </>
        ) : (
          <button
            className="pixel-btn"
            title="Start — move to In Progress"
            onClick={() => onSetStatus(task, 'In Progress')}
          >
            ▶
          </button>
        )}
      </div>
    </li>
  );
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

  // Completed work is history, not a to-do list. On Hold stays visible: it is
  // blocked, not finished, and hiding it would let it rot unseen.
  const tasks = sortTasks((snapshot?.tasks ?? []).filter((t) => t.status !== 'Done'));
  const pager = usePagination(tasks);

  const load = useCallback(async (refresh: boolean) => {
    try {
      setSnapshot(await invoke<Snapshot>('list_my_tasks', { refresh }));
      setError(null);
    } catch (err) {
      setError(err as UiError);
    }
  }, []);

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

  useEffect(() => {
    if (!toast) return;
    const timer = window.setTimeout(() => setToast(null), 2600);
    return () => window.clearTimeout(timer);
  }, [toast]);

  const setStatus = async (task: Task, status: string) => {
    try {
      setSnapshot(
        await invoke<Snapshot>('update_task', {
          recordId: task.record_id,
          patch: { status },
        })
      );
      setToast(status === 'Done' ? `Done: ${task.title}` : `-> ${status}`);
    } catch (err) {
      setError(err as UiError);
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
      pager.reset();
      setToast(`Added: ${title}`);
    } catch (err) {
      setError(err as UiError);
    } finally {
      setAdding(false);
    }
  };

  const pendingIds = new Set(snapshot?.pending_ids ?? []);

  return (
    <div className="app">
      <header className="titlebar" data-tauri-drag-region>
        <span className="titlebar__name" data-tauri-drag-region>
          OMSN
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
      </header>

      {error && (
        <div className="notice notice--error">
          <span>{error.message}</span>
          {error.needs_login && (
            <button className="pixel-btn" onClick={() => void signIn()} disabled={busy}>
              {busy ? '...' : 'SIGN IN'}
            </button>
          )}
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
        {!viewer && !error && <p className="empty">CONNECTING...</p>}

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
              onSetStatus={(t, s) => void setStatus(t, s)}
              onAskDone={setConfirmDone}
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
        <span className="pager__synced">
          {snapshot?.fetched_at_millis ? relativeTime(snapshot.fetched_at_millis) : ''}
        </span>
      </footer>
    </div>
  );
}
