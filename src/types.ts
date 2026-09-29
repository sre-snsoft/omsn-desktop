/** Mirrors the Rust structs in src-tauri/src/. The single definition of task
 *  shape on the frontend — nothing else should describe these fields. */

export interface Person {
  id: string;
  name: string;
}

export interface Viewer {
  open_id: string;
  display_name: string;
}

export interface Task {
  record_id: string;
  title: string;
  status: string;
  owners: Person[];
  priority: string | null;
  category: string | null;
  workstream: string | null;
  remarks: string | null;
  due_date: number | null;
  created: number | null;
  modified: number | null;
}

export interface Snapshot {
  tasks: Task[];
  fetched_at_millis: number;
  pending_ids: string[];
  stale: boolean;
}

export interface UiError {
  kind: string;
  message: string;
  needs_login: boolean;
}

export interface TaskPatch {
  title?: string | null;
  status?: string | null;
  priority?: string | null;
  remarks?: string | null;
  owner_ids?: string[] | null;
}

/** Status values as stored in the Base, in the order they matter at standup. */
export const STATUS_ORDER = ['In Progress', 'This Week', 'On Hold', 'Backlog', 'Done'] as const;

/** Progression markers: empty → half → full → check.
 *  Geometric glyphs read as one family at small sizes, which mixed-weight
 *  emoji do not. Colour carries the state; the shape carries the progress. */
export const STATUS_ICON: Record<string, string> = {
  Backlog: '○',
  'This Week': '◔',
  'In Progress': '◐',
  'On Hold': '◑',
  Done: '●',
};

/** CSS modifier per status, so colour is styled rather than baked in. */
export const STATUS_CLASS: Record<string, string> = {
  Backlog: 'backlog',
  'This Week': 'week',
  'In Progress': 'progress',
  'On Hold': 'hold',
  Done: 'done',
};

/** An In Progress task untouched for this long needs a decision. */
export const STALE_DAYS = 14;

export function daysSince(millis: number | null): number | null {
  if (!millis) return null;
  return Math.floor((Date.now() - millis) / 86_400_000);
}

export function isStale(task: Task): boolean {
  if (task.status !== 'In Progress') return false;
  const days = daysSince(task.modified ?? task.created);
  return days !== null && days >= STALE_DAYS;
}

/** Sort rank for the full Base labels ("P0 - Critical"), lowest first. */
export function priorityRank(priority: string | null): number {
  if (!priority) return 3;
  if (priority.startsWith('P0')) return 0;
  if (priority.startsWith('P1')) return 1;
  if (priority.startsWith('P2')) return 2;
  return 3;
}

/** Group by status in standup order, then by priority, then oldest first. */
export function sortTasks(tasks: Task[]): Task[] {
  const statusRank = (s: string) => {
    const i = (STATUS_ORDER as readonly string[]).indexOf(s);
    return i === -1 ? STATUS_ORDER.length : i;
  };
  return [...tasks].sort((a, b) => {
    const byStatus = statusRank(a.status) - statusRank(b.status);
    if (byStatus !== 0) return byStatus;
    const byPriority = priorityRank(a.priority) - priorityRank(b.priority);
    if (byPriority !== 0) return byPriority;
    return (a.modified ?? a.created ?? 0) - (b.modified ?? b.created ?? 0);
  });
}
