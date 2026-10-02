import { useEffect, useRef, useState } from 'react';
import type { Task } from './types';
import { STATUS_CLASS, STATUS_ICON, daysSince, isStale } from './types';

/** Clicking the marker moves work forward. Completing is deliberately absent:
 *  it removes the row, so it is confirmed rather than one click away. */
const ADVANCE: Record<string, string> = {
  Backlog: 'In Progress',
  'This Week': 'In Progress',
  'On Hold': 'In Progress',
};

/** Matches the add bar, and the Base's own Title column. */
const TITLE_MAX = 200;

export function TaskRow({
  task,
  pending,
  expanded,
  onToggleExpand,
  onSetStatus,
  onAskDone,
  onRename,
  onAskDelete,
}: {
  task: Task;
  pending: boolean;
  expanded: boolean;
  onToggleExpand: (recordId: string) => void;
  onSetStatus: (task: Task, status: string) => void;
  onAskDone: (task: Task) => void;
  /** Resolves false when the Base refused the new title, so the editor can
   *  stay open on the text the user typed rather than discarding it. */
  onRename: (task: Task, title: string) => Promise<boolean>;
  onAskDelete: (task: Task) => void;
}) {
  const age = daysSince(task.modified ?? task.created);
  const stale = isStale(task);
  const inProgress = task.status === 'In Progress';
  const body = useRef<HTMLDivElement>(null);
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState(task.title);

  // Collapsing the row abandons the edit. Nothing is sent: clicking away from
  // a half-typed title must not commit it.
  useEffect(() => {
    if (!expanded) setEditing(false);
  }, [expanded]);

  const startEditing = () => {
    setDraft(task.title);
    setEditing(true);
  };

  const commit = async () => {
    if (await onRename(task, draft)) setEditing(false);
  };

  // A row taller than the page would otherwise expand below the fold. The
  // list scrolls, so bring the row the user just opened back into view.
  useEffect(() => {
    if (expanded) body.current?.scrollIntoView?.({ block: 'nearest' });
  }, [expanded]);

  const toggle = () => {
    // Dragging across the title to select text must not also toggle the row.
    if (window.getSelection()?.isCollapsed === false) return;
    onToggleExpand(task.record_id);
  };

  return (
    <li
      className={`task ${pending ? 'task--pending' : ''} ${expanded ? 'task--expanded' : ''}`}
    >
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

      {/* The expand handler lives here, never on the <li>: the status marker
          and the action buttons are siblings, so no hit area overlaps and
          nothing needs stopPropagation. */}
      <div
        className="task__body"
        ref={body}
        role="button"
        tabIndex={0}
        aria-expanded={expanded}
        onClick={toggle}
        onKeyDown={(e) => {
          if (e.key !== 'Enter' && e.key !== ' ') return;
          e.preventDefault(); // Space would otherwise scroll the list.
          onToggleExpand(task.record_id);
        }}
      >
        {editing ? (
          <input
            className="task__rename"
            value={draft}
            maxLength={TITLE_MAX}
            aria-label="Task title"
            autoFocus
            onChange={(e) => setDraft(e.target.value)}
            // The row's own click and Enter/Space handlers would otherwise
            // collapse the row out from under the input being typed into.
            onClick={(e) => e.stopPropagation()}
            onKeyDown={(e) => {
              e.stopPropagation();
              if (e.key === 'Enter') void commit();
              if (e.key === 'Escape') setEditing(false);
            }}
          />
        ) : (
          <span
            className={`task__title ${expanded ? 'task__title--expanded' : ''}`}
            title={expanded ? undefined : task.title}
          >
            {task.title}
          </span>
        )}
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

      {/* Editing and deleting live here rather than in the hover strip: four
          buttons do not fit a 360px window, and an irreversible delete must
          not sit beside the complete marker. Expanding is already a
          deliberate click, so each is two intentional actions away. */}
      {expanded && (
        <div className="task__drawer">
          {editing ? (
            <>
              <button className="pixel-btn" onClick={() => void commit()}>
                SAVE
              </button>
              <button
                className="pixel-btn pixel-btn--ghost"
                onClick={() => setEditing(false)}
              >
                CANCEL
              </button>
            </>
          ) : (
            <>
              <button className="pixel-btn pixel-btn--ghost" onClick={startEditing}>
                EDIT
              </button>
              <button
                className="pixel-btn pixel-btn--danger"
                onClick={() => onAskDelete(task)}
              >
                DELETE
              </button>
            </>
          )}
        </div>
      )}
    </li>
  );
}
