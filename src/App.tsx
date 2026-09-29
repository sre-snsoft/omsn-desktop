import { useCallback, useEffect, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import type { Snapshot, Task, UiError, Viewer } from './types';
import { STATUS_ICON, daysSince, isStale, sortTasks } from './types';
import { usePagination } from './usePagination';
import './App.css';

/** How often to re-read the Base. Polling only while focused keeps the app
 *  quiet in the background and is refreshed immediately on refocus. */
const POLL_INTERVAL_MS = 20_000;

const NEXT_STATUS: Record<string, string> = {
  Backlog: 'In Progress',
  'This Week': 'In Progress',
  'In Progress': 'Done',
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
  onAdvance,
}: {
  task: Task;
  pending: boolean;
  onAdvance: (task: Task) => void;
}) {
  const age = daysSince(task.modified ?? task.created);
  const stale = isStale(task);

  return (
    <li className={`task ${pending ? 'task--pending' : ''}`}>
      <button
        className="task__status"
        title={`${task.status} → ${NEXT_STATUS[task.status] ?? 'Backlog'}`}
        onClick={() => onAdvance(task)}
      >
        {STATUS_ICON[task.status] ?? '•'}
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
          {task.workstream && <span className="chip">{task.workstream}</span>}
          {age !== null && (
            <span className={stale ? 'age age--stale' : 'age'}>
              {age}d{stale ? ' · needs a decision' : ''}
            </span>
          )}
        </span>
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

  // Completed work is history, not a to-do list — it never appears here.
  // On Hold does: it is blocked, not finished, and hiding it would let it rot
  // unseen, which is exactly what the weekly "still blocked?" nudge exists for.
  const tasks = sortTasks((snapshot?.tasks ?? []).filter((t) => t.status !== 'Done'));
  const pager = usePagination(tasks);

  const load = useCallback(async (refresh: boolean) => {
    try {
      const snap = await invoke<Snapshot>('list_my_tasks', { refresh });
      setSnapshot(snap);
      setError(null);
    } catch (err) {
      setError(err as UiError);
    }
  }, []);

  const signIn = useCallback(async () => {
    setBusy(true);
    try {
      const who = await invoke<Viewer>('sign_in');
      setViewer(who);
      setError(null);
      await load(true);
    } catch (err) {
      setError(err as UiError);
    } finally {
      setBusy(false);
    }
  }, [load]);

  useEffect(() => {
    void signIn();
  }, [signIn]);

  // Poll only while the window has focus, and refresh the moment it regains it.
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

  const advance = async (task: Task) => {
    const status = NEXT_STATUS[task.status] ?? 'Backlog';
    try {
      const snap = await invoke<Snapshot>('update_task', {
        recordId: task.record_id,
        patch: { status },
      });
      setSnapshot(snap);
      // A completed task leaves the list, so say so — otherwise it just
      // disappears and the click looks like it did something unintended.
      setToast(status === 'Done' ? `Completed “${task.title}”` : `Moved to ${status}`);
    } catch (err) {
      setError(err as UiError);
    }
  };

  useEffect(() => {
    if (!toast) return;
    const timer = window.setTimeout(() => setToast(null), 2600);
    return () => window.clearTimeout(timer);
  }, [toast]);

  const pendingIds = new Set(snapshot?.pending_ids ?? []);

  return (
    <div className="app">
      {/* The whole bar is the drag handle — the window has no native titlebar. */}
      <header className="titlebar" data-tauri-drag-region>
        <span className="titlebar__name" data-tauri-drag-region>
          OMSN
        </span>
        <span className="titlebar__count" data-tauri-drag-region>
          {viewer ? `${tasks.length} active` : ''}
        </span>
        <button className="titlebar__btn" onClick={() => void load(true)} title="Refresh">
          ↻
        </button>
      </header>

      {error && (
        <div className="notice notice--error">
          <span>{error.message}</span>
          {error.needs_login && (
            <button onClick={() => void signIn()} disabled={busy}>
              Sign in
            </button>
          )}
        </div>
      )}

      {snapshot?.stale && !error && (
        <div className="notice notice--warn">
          Showing the last known tasks — couldn’t reach Lark.
        </div>
      )}

      <main className="list">
        {!viewer && !error && <p className="empty">Connecting…</p>}

        {viewer && tasks.length === 0 && (
          <p className="empty">
            Nothing assigned to you right now.
            <br />
            <span className="empty__sub">That’s a legitimate state, not a bug.</span>
          </p>
        )}

        <ul>
          {pager.visible.map((task) => (
            <TaskRow
              key={task.record_id}
              task={task}
              pending={pendingIds.has(task.record_id)}
              onAdvance={(t) => void advance(t)}
            />
          ))}
        </ul>
      </main>

      {toast && <div className="toast">{toast}</div>}

      <footer className="pager">
        <button onClick={pager.prev} disabled={!pager.hasPrev} title="Previous page">
          ‹
        </button>
        <span className="pager__label">
          {pager.pageCount > 1 ? `${pager.page + 1} / ${pager.pageCount}` : ''}
        </span>
        <button onClick={pager.next} disabled={!pager.hasNext} title="Next page">
          ›
        </button>
        <span className="pager__synced">
          {snapshot?.fetched_at_millis ? relativeTime(snapshot.fetched_at_millis) : ''}
        </span>
      </footer>
    </div>
  );
}
