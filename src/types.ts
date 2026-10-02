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
  /** The day the task was completed, written when a client sets Done.
   *  Null for anything finished by hand in the Lark UI — not guessed at. */
  completed_date: number | null;
  created: number | null;
  modified: number | null;
}

/** A write the Base refused. The command that sent it returned long before
 *  the answer arrived, so the rejection travels back in a later snapshot. */
export interface WriteFailure {
  seq: number;
  record_id: string;
  message: string;
}

export interface Snapshot {
  tasks: Task[];
  fetched_at_millis: number;
  pending_ids: string[];
  write_failures: WriteFailure[];
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
  /** Epoch millis, and decided in Rust. The UI never sets this. */
  completed_date?: number | null;
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

/** A row held in place after the user changed its status.
 *
 *  `sortTasks` ranks Backlog 4th and orders within a group by `modified`
 *  ascending, so the row you just touched has the newest timestamp and lands
 *  at the very bottom of the list — off the current page as soon as there is
 *  more than one page. The user sees it vanish with no clue where it went.
 *
 *  Rather than change the sort (which standup depends on), the row is pinned
 *  to the slot it occupied until the next poll re-reads the server. */
export interface Pin {
  recordId: string;
  /** Index in the full sorted list at the moment of the change. */
  index: number;
  /** The snapshot the pin was taken against; a newer poll releases it. */
  fetchedAt: number;
}

/** Move the pinned row back to the index it held. Returns a new array. */
export function pinTask(tasks: Task[], pin: Pin | null): Task[] {
  if (!pin) return tasks;
  const from = tasks.findIndex((t) => t.record_id === pin.recordId);
  if (from === -1) return tasks;
  const to = Math.max(0, Math.min(pin.index, tasks.length - 1));
  if (from === to) return tasks;
  const next = [...tasks];
  const [moved] = next.splice(from, 1);
  next.splice(to, 0, moved);
  return next;
}

/** Up to two letters standing in for a name, Penguin-style ("v0.1.0 . AC"). */
export function initials(displayName: string | null | undefined): string {
  const words = (displayName ?? '').trim().split(/\s+/).filter(Boolean);
  if (words.length === 0) return '';
  if (words.length === 1) return words[0].slice(0, 2).toUpperCase();
  return (words[0][0] + words[words.length - 1][0]).toUpperCase();
}

/** Wall-clock time in the user's own locale and 12/24-hour convention. */
export function formatClock(at: Date, locale?: string): string {
  return at.toLocaleTimeString(locale, { hour: 'numeric', minute: '2-digit' });
}
