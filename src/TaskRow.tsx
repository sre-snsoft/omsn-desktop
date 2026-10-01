import { useEffect, useRef } from 'react';
import type { Task } from './types';
import { STATUS_CLASS, STATUS_ICON, daysSince, isStale } from './types';

/** Clicking the marker moves work forward. Completing is deliberately absent:
 *  it removes the row, so it is confirmed rather than one click away. */
const ADVANCE: Record<string, string> = {
  Backlog: 'In Progress',
  'This Week': 'In Progress',
  'On Hold': 'In Progress',
};

export function TaskRow({
  task,
  pending,
  expanded,
  onToggleExpand,
  onSetStatus,
  onAskDone,
}: {
  task: Task;
  pending: boolean;
  expanded: boolean;
  onToggleExpand: (recordId: string) => void;
  onSetStatus: (task: Task, status: string) => void;
  onAskDone: (task: Task) => void;
}) {
  const age = daysSince(task.modified ?? task.created);
  const stale = isStale(task);
  const inProgress = task.status === 'In Progress';
  const body = useRef<HTMLDivElement>(null);

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
        <span
          className={`task__title ${expanded ? 'task__title--expanded' : ''}`}
          title={expanded ? undefined : task.title}
        >
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
