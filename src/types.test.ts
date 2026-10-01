import { describe, expect, it } from 'vitest';
import type { Task } from './types';
import { STATUS_ICON, daysSince, isStale, priorityRank, sortTasks } from './types';

function task(over: Partial<Task> = {}): Task {
  return {
    record_id: 'r1',
    title: 'a task',
    status: 'Backlog',
    owners: [],
    priority: null,
    category: null,
    workstream: null,
    remarks: null,
    due_date: null,
    created: null,
    modified: null,
    ...over,
  };
}

const DAY = 86_400_000;

describe('priorityRank', () => {
  it('orders the real Base labels, not bare P1', () => {
    expect(priorityRank('P0 - Critical')).toBe(0);
    expect(priorityRank('P1 - Important')).toBe(1);
    expect(priorityRank('P2 - Normal')).toBe(2);
  });

  it('sorts unset priority last so it never outranks real work', () => {
    expect(priorityRank(null)).toBe(3);
    expect(priorityRank('')).toBe(3);
    expect(priorityRank('something else')).toBe(3);
  });
});

describe('isStale', () => {
  it('flags In Progress work untouched past the threshold', () => {
    expect(isStale(task({ status: 'In Progress', modified: Date.now() - 20 * DAY }))).toBe(true);
  });

  it('leaves recently touched work alone however old the record is', () => {
    expect(
      isStale(task({ status: 'In Progress', created: Date.now() - 300 * DAY, modified: Date.now() - DAY }))
    ).toBe(false);
  });

  it('never flags Backlog — unscheduled is not rotting', () => {
    expect(isStale(task({ status: 'Backlog', modified: Date.now() - 400 * DAY }))).toBe(false);
  });

  it('never flags On Hold, which is blocked rather than stalled', () => {
    expect(isStale(task({ status: 'On Hold', modified: Date.now() - 400 * DAY }))).toBe(false);
  });

  it('cannot flag a task with no timestamps at all', () => {
    expect(isStale(task({ status: 'In Progress' }))).toBe(false);
  });
});

describe('daysSince', () => {
  it('returns null rather than a bogus age when there is no timestamp', () => {
    expect(daysSince(null)).toBeNull();
    expect(daysSince(0)).toBeNull();
  });

  it('counts whole days', () => {
    expect(daysSince(Date.now() - 3 * DAY)).toBe(3);
  });
});

describe('sortTasks', () => {
  it('puts In Progress above Backlog so page one is what matters', () => {
    const got = sortTasks([
      task({ record_id: 'b', status: 'Backlog' }),
      task({ record_id: 'p', status: 'In Progress' }),
    ]);
    expect(got.map((t) => t.record_id)).toEqual(['p', 'b']);
  });

  it('orders by priority inside a status group', () => {
    const got = sortTasks([
      task({ record_id: 'p2', status: 'In Progress', priority: 'P2 - Normal' }),
      task({ record_id: 'p0', status: 'In Progress', priority: 'P0 - Critical' }),
      task({ record_id: 'p1', status: 'In Progress', priority: 'P1 - Important' }),
    ]);
    expect(got.map((t) => t.record_id)).toEqual(['p0', 'p1', 'p2']);
  });

  it('breaks ties oldest first, so neglected work surfaces', () => {
    const got = sortTasks([
      task({ record_id: 'new', status: 'In Progress', modified: Date.now() }),
      task({ record_id: 'old', status: 'In Progress', modified: Date.now() - 50 * DAY }),
    ]);
    expect(got[0].record_id).toBe('old');
  });

  it('does not mutate the input array', () => {
    const input = [
      task({ record_id: 'b', status: 'Backlog' }),
      task({ record_id: 'p', status: 'In Progress' }),
    ];
    sortTasks(input);
    expect(input.map((t) => t.record_id)).toEqual(['b', 'p']);
  });

  it('keeps an unknown status visible rather than dropping it', () => {
    const got = sortTasks([task({ status: 'Something New' })]);
    expect(got).toHaveLength(1);
  });
});

describe('STATUS_ICON', () => {
  it('covers every status the Base defines', () => {
    for (const s of ['Backlog', 'This Week', 'In Progress', 'On Hold', 'Done']) {
      expect(STATUS_ICON[s], `${s} needs a marker`).toBeTruthy();
    }
  });
});
