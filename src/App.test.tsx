/**
 * First-run and sign-in state-machine tests for App.tsx.
 *
 * The question these answer: when a teammate launches OMSN for the very first
 * time, is there always something on screen that tells them what to do?
 */
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, render, screen, waitFor, fireEvent, act } from '@testing-library/react';
import type { Snapshot, Task, UiError, Viewer } from './types';

vi.mock('@tauri-apps/api/core', () => ({ invoke: vi.fn() }));
vi.mock('@tauri-apps/api/app', () => ({ getVersion: vi.fn(async () => '0.1.0') }));
/** Rust emits `omsn://write-settled` when a write settles. The fake hands the
 *  handler back so a test can fire the event the way Rust would. */
vi.mock('@tauri-apps/api/event', () => ({
  listen: vi.fn(async (_event: string, handler: () => void) => {
    settleListeners.push(handler);
    return () => {
      settleListeners = settleListeners.filter((h) => h !== handler);
    };
  }),
}));
let settleListeners: Array<() => void> = [];
function emitWriteSettled() {
  for (const handler of [...settleListeners]) handler();
}
// eslint-disable-next-line import/first
import { invoke } from '@tauri-apps/api/core';
// eslint-disable-next-line import/first
import App from './App';

const invokeMock = vi.mocked(invoke);

const VIEWER: Viewer = { open_id: 'ou_me', display_name: 'Adrian' };

function task(over: Partial<Task> = {}): Task {
  return {
    record_id: 'rec1',
    title: 'Ship the OAuth flow',
    status: 'In Progress',
    owners: [{ id: 'ou_me', name: 'Adrian' }],
    priority: 'P1 - High',
    category: null,
    workstream: null,
    remarks: null,
    due_date: null,
    completed_date: null,
    created: Date.now(),
    modified: Date.now(),
    ...over,
  };
}

function snapshot(tasks: Task[] = [], over: Partial<Snapshot> = {}): Snapshot {
  return {
    tasks,
    fetched_at_millis: Date.now(),
    pending_ids: [],
    write_failures: [],
    stale: false,
    ...over,
  };
}

/** What the Rust side actually returns when there is no token file at all. */
const NO_SESSION: UiError = {
  kind: 'auth',
  message: 'Sign-in failed: No saved session. Sign in to continue.',
  needs_login: true,
};

/** What it returns when ~/.config/omsn/desktop.env is missing — the state
 *  every one of the 13 teammates is in before they are set up. */
const NO_CONFIG: UiError = {
  kind: 'config',
  message: 'Configuration problem: Cannot read /Users/x/.config/omsn/desktop.env: No such file',
  needs_login: false,
};

const OFFLINE: UiError = {
  kind: 'network',
  message: 'Could not reach Lark. Check your connection and try again.',
  needs_login: false,
};

/** Every control a user could press to start a sign-in. */
function signInAffordances() {
  return screen
    .queryAllByRole('button')
    .filter((b) => /sign in|wait/i.test(b.textContent ?? ''));
}

/** Step past the 2 s refresh floor, so a test can trigger a real refresh. */
function skipRefreshFloor() {
  const real = Date.now();
  return vi.spyOn(Date, 'now').mockImplementation(() => real + 10_000);
}

beforeEach(() => {
  invokeMock.mockReset();
  settleListeners = [];
});
afterEach(() => {
  cleanup();
});

describe('first launch', () => {
  it('shows a usable sign-in affordance when there is no saved session', async () => {
    invokeMock.mockImplementation(async (cmd) => {
      if (cmd === 'sign_in') throw NO_SESSION;
      throw NO_SESSION;
    });

    render(<App />);

    await waitFor(() => expect(signInAffordances().length).toBeGreaterThan(0));
    expect(screen.queryByText(/CONNECTING/i)).toBeNull();
  });

  it('greets a brand-new user with the welcome screen, not a failure notice', async () => {
    invokeMock.mockImplementation(async (cmd) => {
      if (cmd === 'sign_in') throw NO_SESSION;
      throw NO_SESSION;
    });

    render(<App />);

    await waitFor(() => expect(signInAffordances().length).toBeGreaterThan(0));
    expect(
      screen.queryByText('OMSN DESKTOP'),
      'the welcome screen is gated on `!error`, and the first sign_in always ' +
        'errors, so a new teammate sees a red failure notice instead'
    ).not.toBeNull();
  });

  it('never leaves the user with no way to sign in when the config is missing', async () => {
    invokeMock.mockImplementation(async () => {
      throw NO_CONFIG;
    });

    render(<App />);

    await waitFor(() => expect(screen.getByText(NO_CONFIG.message)).toBeInTheDocument());
    expect(screen.queryByText(/CONNECTING/i)).toBeNull();
    expect(
      signInAffordances(),
      'needs_login is false for a config error, so no SIGN IN button renders ' +
        'and the welcome screen is suppressed by `error` — dead end'
    ).not.toHaveLength(0);
  });

  it('never leaves the user with no way to sign in when the first launch is offline', async () => {
    invokeMock.mockImplementation(async () => {
      throw OFFLINE;
    });

    render(<App />);

    await waitFor(() => expect(screen.getByText(OFFLINE.message)).toBeInTheDocument());
    expect(
      signInAffordances(),
      'a transient network blip on first launch strands the user'
    ).not.toHaveLength(0);
  });

  it('does not claim "NOTHING ASSIGNED" before anyone has signed in', async () => {
    invokeMock.mockImplementation(async () => {
      throw NO_SESSION;
    });

    render(<App />);

    await waitFor(() => expect(signInAffordances().length).toBeGreaterThan(0));
    expect(screen.queryByText(/NOTHING ASSIGNED/i)).toBeNull();
  });

  it('always renders something actionable, whatever the first error is', async () => {
    for (const err of [NO_SESSION, NO_CONFIG, OFFLINE]) {
      invokeMock.mockReset();
      invokeMock.mockImplementation(async () => {
        throw err;
      });
      const { unmount } = render(<App />);
      await waitFor(() => expect(screen.getByText(err.message)).toBeInTheDocument());
      const actionable =
        signInAffordances().length > 0 || screen.queryByText('OMSN DESKTOP') !== null;
      expect(actionable, `kind=${err.kind} leaves no sign-in path on screen`).toBe(true);
      unmount();
      cleanup();
    }
  });
});

describe('the browser consent flow', () => {
  it('shows progress for the whole time the browser is open', async () => {
    let release: (v: Viewer) => void = () => {};
    const consent = new Promise<Viewer>((res) => {
      release = res;
    });
    invokeMock.mockImplementation(async (cmd) => {
      if (cmd === 'sign_in') throw NO_SESSION;
      if (cmd === 'authorize') return consent;
      if (cmd === 'list_my_tasks') return snapshot([task()]);
      throw NO_SESSION;
    });

    render(<App />);
    await waitFor(() => expect(signInAffordances().length).toBeGreaterThan(0));

    fireEvent.click(signInAffordances()[0]);

    // oauth::wait_for_code can block for up to 180 seconds here.
    await waitFor(() => expect(screen.getByText(/CONNECTING/i)).toBeInTheDocument());
    expect(signInAffordances().every((b) => (b as HTMLButtonElement).disabled)).toBe(true);

    await act(async () => {
      release(VIEWER);
      await consent;
    });
    await waitFor(() => expect(screen.getByText('Ship the OAuth flow')).toBeInTheDocument());
  });

  it('recovers from a timed-out consent instead of staying busy forever', async () => {
    const timedOut: UiError = {
      kind: 'auth',
      message: 'Sign-in failed: Sign-in timed out. Please try again.',
      needs_login: true,
    };
    invokeMock.mockImplementation(async (cmd) => {
      if (cmd === 'sign_in') throw NO_SESSION;
      if (cmd === 'authorize') throw timedOut;
      throw NO_SESSION;
    });

    render(<App />);
    await waitFor(() => expect(signInAffordances().length).toBeGreaterThan(0));
    fireEvent.click(signInAffordances()[0]);

    await waitFor(() => expect(screen.getByText(timedOut.message)).toBeInTheDocument());
    await waitFor(() =>
      expect(signInAffordances().some((b) => !(b as HTMLButtonElement).disabled)).toBe(true)
    );
  });

  it('does not strand the user when consent succeeds but the first load fails', async () => {
    invokeMock.mockImplementation(async (cmd) => {
      if (cmd === 'sign_in') throw NO_SESSION;
      if (cmd === 'authorize') return VIEWER;
      if (cmd === 'list_my_tasks') throw OFFLINE;
      throw OFFLINE;
    });

    render(<App />);
    await waitFor(() => expect(signInAffordances().length).toBeGreaterThan(0));
    fireEvent.click(signInAffordances()[0]);

    await waitFor(() => expect(screen.getByText(OFFLINE.message)).toBeInTheDocument());
    // Signed in, so the add bar must be usable even though the list failed.
    expect(screen.getByPlaceholderText('new task...')).toBeInTheDocument();
  });

  it('shows the empty state, not a sign-in prompt, for a user with zero tasks', async () => {
    invokeMock.mockImplementation(async (cmd) => {
      if (cmd === 'sign_in') return VIEWER;
      if (cmd === 'list_my_tasks') return snapshot([]);
      throw new Error(`unexpected ${cmd}`);
    });

    render(<App />);

    await waitFor(() => expect(screen.getByText(/NOTHING ASSIGNED/i)).toBeInTheDocument());
    expect(signInAffordances()).toHaveLength(0);
    expect(screen.queryByText('OMSN DESKTOP')).toBeNull();
  });

  it('renders untrusted text from a sign-in error as text, never as markup', async () => {
    const hostile: UiError = {
      kind: 'auth',
      // wait_for_code pastes the redirect's `error` parameter in verbatim.
      message: 'Sign-in failed: Lark denied the sign-in (<img src=x onerror="alert(1)">)',
      needs_login: true,
    };
    invokeMock.mockImplementation(async () => {
      throw hostile;
    });

    render(<App />);
    await waitFor(() => expect(screen.getByText(hostile.message)).toBeInTheDocument());
    expect(document.querySelector('img'), 'the message was interpreted as HTML').toBeNull();
  });

  it('does not run two sign-ins at once from a double click', async () => {
    let authorizeCalls = 0;
    invokeMock.mockImplementation(async (cmd) => {
      if (cmd === 'sign_in') throw NO_SESSION;
      if (cmd === 'authorize') {
        authorizeCalls += 1;
        await new Promise((r) => setTimeout(r, 50));
        return VIEWER;
      }
      if (cmd === 'list_my_tasks') return snapshot([]);
      throw NO_SESSION;
    });

    render(<App />);
    await waitFor(() => expect(signInAffordances().length).toBeGreaterThan(0));
    const button = signInAffordances()[0];
    fireEvent.click(button);
    fireEvent.click(button);

    await waitFor(() => expect(screen.getByText(/NOTHING ASSIGNED/i)).toBeInTheDocument());
    expect(
      authorizeCalls,
      'a second authorize would open a second browser tab and race the redirect port'
    ).toBe(1);
  });
});

describe('escaping a config dead end', () => {
  it('offers a real sign-in even when the config file is missing', async () => {
    // Previously the welcome screen was gated on there being no error, so the
    // most likely first-run state — no desktop.env yet — showed a bare red
    // banner with nothing to click. The user was stranded.
    const calls: string[] = [];
    invokeMock.mockImplementation(async (cmd) => {
      calls.push(cmd as string);
      throw NO_CONFIG;
    });

    render(<App />);
    await waitFor(() => expect(screen.getByText(NO_CONFIG.message)).toBeInTheDocument());

    expect(
      signInAffordances().length,
      'a config error must still leave a way to sign in'
    ).toBeGreaterThan(0);
    // And it names the file to fix, since signing in cannot repair config.
    expect(screen.getByText(/^Check .*desktop\.env$/)).toBeInTheDocument();
  });
});

describe('a status change must land instantly (R5)', () => {
  const FETCHED = 1_760_000_000_000;

  it('marks the row in-flight until Rust confirms the write', async () => {
    invokeMock.mockImplementation(async (cmd) => {
      if (cmd === 'sign_in') return VIEWER;
      if (cmd === 'list_my_tasks') return snapshot([task()]);
      if (cmd === 'update_task') {
        return snapshot([task({ status: 'Backlog' })], { pending_ids: ['rec1'] });
      }
      throw new Error(`unexpected ${cmd}`);
    });

    render(<App />);
    await waitFor(() => expect(screen.getByText('Ship the OAuth flow')).toBeInTheDocument());

    fireEvent.click(screen.getByTitle('Back to Backlog'));

    await waitFor(() =>
      expect(
        document.querySelector('.task--pending'),
        '.task--pending is styled but never rendered unless the overlay survives'
      ).not.toBeNull()
    );
  });

  it('tells the user when the Base refuses the change, and shows the real status', async () => {
    // The command answers before the PUT does, so a rejection can only come
    // back in a later snapshot. Losing it would leave the user believing a
    // change they can no longer see actually applied.
    let settled = false;
    invokeMock.mockImplementation(async (cmd) => {
      if (cmd === 'sign_in') return VIEWER;
      if (cmd === 'update_task') {
        settled = true;
        return snapshot([task({ status: 'Backlog' })], {
          pending_ids: ['rec1'],
          fetched_at_millis: FETCHED,
        });
      }
      if (cmd === 'list_my_tasks') {
        if (!settled) return snapshot([task()], { fetched_at_millis: FETCHED });
        return snapshot([task({ status: 'In Progress' })], {
          fetched_at_millis: FETCHED,
          write_failures: [
            {
              seq: 1,
              record_id: 'rec1',
              message: 'You do not have permission to do that in this Base.',
            },
          ],
        });
      }
      throw new Error(`unexpected ${cmd}`);
    });

    render(<App />);
    await waitFor(() => expect(screen.getByText('Ship the OAuth flow')).toBeInTheDocument());
    fireEvent.click(screen.getByTitle('Back to Backlog'));

    await waitFor(
      () => expect(screen.getByText(/Change not saved/)).toBeInTheDocument(),
      { timeout: 3000 }
    );
    expect(screen.getByText(/do not have permission/)).toBeInTheDocument();
    // And the marker is back to the truth, not the optimistic value.
    await waitFor(() => expect(screen.getByTitle('In Progress')).toBeInTheDocument());
  });

  it('keeps a rejection on screen when a later poll succeeds', async () => {
    // `load` clears `error` on success. A rejection reported through the same
    // channel would be erased 400ms later by the settle poll.
    invokeMock.mockImplementation(async (cmd) => {
      if (cmd === 'sign_in') return VIEWER;
      if (cmd === 'update_task') {
        // Still pending, so the settle poll runs — and succeeds, clearing
        // `error`. The rejection must not go with it.
        return snapshot([task({ status: 'In Progress' })], {
          fetched_at_millis: FETCHED,
          pending_ids: ['rec1'],
          write_failures: [{ seq: 7, record_id: 'rec1', message: 'Lark is rate limiting us.' }],
        });
      }
      if (cmd === 'list_my_tasks') return snapshot([task()], { fetched_at_millis: FETCHED });
      throw new Error(`unexpected ${cmd}`);
    });

    render(<App />);
    await waitFor(() => expect(screen.getByText('Ship the OAuth flow')).toBeInTheDocument());
    fireEvent.click(screen.getByTitle('Back to Backlog'));

    await waitFor(() => expect(screen.getByText(/Change not saved/)).toBeInTheDocument());
    await act(async () => {
      await new Promise((r) => setTimeout(r, 600));
    });
    expect(screen.getByText(/Change not saved/)).toBeInTheDocument();
  });

  it('does not let the row it just moved fall off the current page', async () => {
    // The real symptom: Backlog ranks 4th and sorts by `modified` ascending,
    // so the touched row jumps to the bottom of the whole list — past the end
    // of page 1.
    const many = Array.from({ length: 10 }, (_, i) =>
      task({
        record_id: `rec${i}`,
        title: `task number ${i}`,
        status: 'In Progress',
        modified: 1_000 + i,
      })
    );
    const moved = many.map((t) =>
      t.record_id === 'rec0' ? { ...t, status: 'Backlog', modified: 9_999_999 } : t
    );

    let changed = false;
    invokeMock.mockImplementation(async (cmd) => {
      if (cmd === 'sign_in') return VIEWER;
      if (cmd === 'update_task') {
        changed = true;
        return snapshot(moved, { pending_ids: ['rec0'], fetched_at_millis: FETCHED });
      }
      if (cmd === 'list_my_tasks') {
        return snapshot(changed ? moved : many, { fetched_at_millis: FETCHED });
      }
      throw new Error(`unexpected ${cmd}`);
    });

    render(<App />);
    await waitFor(() => expect(screen.getByText('task number 0')).toBeInTheDocument());
    expect(screen.getByText('1/2'), 'the list must span more than one page').toBeInTheDocument();

    fireEvent.click(screen.getAllByTitle('Back to Backlog')[0]);

    await waitFor(() => expect(screen.getByTitle('Backlog')).toBeInTheDocument());
    expect(
      screen.getByText('task number 0'),
      'the row the user just touched vanished off the page'
    ).toBeInTheDocument();
  });
});

describe('clicking a row to read a long title (R3)', () => {
  const LONG = 'Use AI to map Pulsar producers, topics and consumers across every repo';

  function signedInWith(tasks: Task[]) {
    invokeMock.mockImplementation(async (cmd) => {
      if (cmd === 'sign_in') return VIEWER;
      if (cmd === 'list_my_tasks') return snapshot(tasks);
      if (cmd === 'update_task') return snapshot(tasks);
      throw new Error(`unexpected ${cmd}`);
    });
  }

  it('expands the clicked row and collapses it again', async () => {
    signedInWith([task({ title: LONG })]);
    render(<App />);
    await waitFor(() => expect(screen.getByText(LONG)).toBeInTheDocument());

    const body = screen.getByText(LONG).closest('.task__body') as HTMLElement;
    expect(body).toHaveAttribute('aria-expanded', 'false');

    fireEvent.click(body);
    expect(body).toHaveAttribute('aria-expanded', 'true');
    expect(body.querySelector('.task__title--expanded')).not.toBeNull();

    fireEvent.click(body);
    expect(body).toHaveAttribute('aria-expanded', 'false');
  });

  it('expands at most one row at a time', async () => {
    signedInWith([
      task({ record_id: 'r1', title: 'first long title' }),
      task({ record_id: 'r2', title: 'second long title' }),
    ]);
    render(<App />);
    await waitFor(() => expect(screen.getByText('first long title')).toBeInTheDocument());

    const first = screen.getByText('first long title').closest('.task__body') as HTMLElement;
    const second = screen.getByText('second long title').closest('.task__body') as HTMLElement;

    fireEvent.click(first);
    fireEvent.click(second);

    expect(first).toHaveAttribute('aria-expanded', 'false');
    expect(second).toHaveAttribute('aria-expanded', 'true');
    expect(document.querySelectorAll('.task__title--expanded')).toHaveLength(1);
  });

  it('is operable from the keyboard', async () => {
    signedInWith([task({ title: LONG })]);
    render(<App />);
    await waitFor(() => expect(screen.getByText(LONG)).toBeInTheDocument());
    const body = screen.getByText(LONG).closest('.task__body') as HTMLElement;

    expect(body).toHaveAttribute('tabindex', '0');
    fireEvent.keyDown(body, { key: 'Enter' });
    expect(body).toHaveAttribute('aria-expanded', 'true');
    fireEvent.keyDown(body, { key: ' ' });
    expect(body).toHaveAttribute('aria-expanded', 'false');
  });

  it('leaves the status marker doing exactly what it did before', async () => {
    const calls: unknown[] = [];
    invokeMock.mockImplementation(async (cmd, args) => {
      if (cmd === 'sign_in') return VIEWER;
      if (cmd === 'list_my_tasks') return snapshot([task({ status: 'Backlog' })]);
      if (cmd === 'update_task') {
        calls.push(args);
        return snapshot([task({ status: 'In Progress' })]);
      }
      throw new Error(`unexpected ${cmd}`);
    });
    render(<App />);
    await waitFor(() => expect(screen.getByTitle('Backlog')).toBeInTheDocument());

    fireEvent.click(screen.getByTitle('Backlog'));

    await waitFor(() => expect(calls).toHaveLength(1));
    expect(calls[0]).toEqual({ recordId: 'rec1', patch: { status: 'In Progress' } });
    // Advancing status is not also an expand.
    const body = screen.getByText('Ship the OAuth flow').closest('.task__body') as HTMLElement;
    expect(body).toHaveAttribute('aria-expanded', 'false');
  });

  it('does not toggle when the click was the end of a text selection', async () => {
    signedInWith([task({ title: LONG })]);
    render(<App />);
    await waitFor(() => expect(screen.getByText(LONG)).toBeInTheDocument());
    const body = screen.getByText(LONG).closest('.task__body') as HTMLElement;

    const selection = vi
      .spyOn(window, 'getSelection')
      .mockReturnValue({ isCollapsed: false } as Selection);
    fireEvent.click(body);
    expect(body).toHaveAttribute('aria-expanded', 'false');

    selection.mockRestore();
    fireEvent.click(body);
    expect(body).toHaveAttribute('aria-expanded', 'true');
  });

  it('stays expanded when a poll replaces the snapshot', async () => {
    // Expansion is keyed on record_id, so a fresh snapshot of the same row
    // must not snap it shut while someone is reading it.
    let reads = 0;
    invokeMock.mockImplementation(async (cmd) => {
      if (cmd === 'sign_in') return VIEWER;
      if (cmd === 'list_my_tasks') {
        reads += 1;
        return snapshot([task({ title: LONG, modified: Date.now() + reads })]);
      }
      throw new Error(`unexpected ${cmd}`);
    });
    render(<App />);
    await waitFor(() => expect(screen.getByText(LONG)).toBeInTheDocument());

    fireEvent.click(screen.getByText(LONG).closest('.task__body') as HTMLElement);
    expect(screen.getByText(LONG).closest('.task__body')).toHaveAttribute(
      'aria-expanded',
      'true'
    );

    // The focus handler only polls while the window really has focus, and a
    // refresh inside the 2 s floor is dropped as a repeat of the last one.
    const focused = vi.spyOn(document, 'hasFocus').mockReturnValue(true);
    const clock = skipRefreshFloor();
    await act(async () => {
      window.dispatchEvent(new Event('focus'));
      await new Promise((r) => setTimeout(r, 20));
    });
    clock.mockRestore();
    focused.mockRestore();

    expect(reads).toBeGreaterThan(1);
    expect(screen.getByText(LONG).closest('.task__body')).toHaveAttribute(
      'aria-expanded',
      'true'
    );
  });

  it('forgets the expansion when the page changes', async () => {
    const many = Array.from({ length: 10 }, (_, i) =>
      task({ record_id: `rec${i}`, title: `task number ${i}` })
    );
    signedInWith(many);
    render(<App />);
    await waitFor(() => expect(screen.getByText('task number 0')).toBeInTheDocument());

    const body = screen.getByText('task number 0').closest('.task__body') as HTMLElement;
    fireEvent.click(body);
    expect(body).toHaveAttribute('aria-expanded', 'true');

    fireEvent.click(screen.getByText('›'));

    await waitFor(() => expect(screen.getByText('2/2')).toBeInTheDocument());
    expect(document.querySelectorAll('.task__title--expanded')).toHaveLength(0);
  });
});

describe('version and clock (R4)', () => {
  it('shows the running version and the viewer initials in the footer', async () => {
    invokeMock.mockImplementation(async (cmd) => {
      if (cmd === 'sign_in') return VIEWER;
      if (cmd === 'list_my_tasks') return snapshot([task()]);
      throw new Error(`unexpected ${cmd}`);
    });

    render(<App />);
    await waitFor(() => expect(screen.getByText('v0.1.0 · AD')).toBeInTheDocument());
  });

  it('shows the version with no initials before sign-in', async () => {
    invokeMock.mockImplementation(async () => {
      throw NO_SESSION;
    });

    render(<App />);
    await waitFor(() => expect(screen.getByText('v0.1.0')).toBeInTheDocument());
  });

  it('shows a clock in the draggable part of the titlebar', async () => {
    invokeMock.mockImplementation(async (cmd) => {
      if (cmd === 'sign_in') return VIEWER;
      if (cmd === 'list_my_tasks') return snapshot([]);
      throw new Error(`unexpected ${cmd}`);
    });

    render(<App />);
    const clock = await waitFor(() => {
      const el = document.querySelector('.titlebar__clock');
      expect(el?.textContent ?? '').toMatch(/\d{1,2}[:.]\d{2}/);
      return el as HTMLElement;
    });
    // Dragging the window must still work from the clock.
    expect(clock).toHaveAttribute('data-tauri-drag-region');
  });
});

describe('a refresh must not cost a whole table walk (R6)', () => {
  function signedIn(tasks: Task[], extra: Partial<Snapshot> = {}) {
    const reads: boolean[] = [];
    invokeMock.mockImplementation(async (cmd, args) => {
      if (cmd === 'sign_in') return VIEWER;
      if (cmd === 'list_my_tasks') {
        reads.push((args as { refresh: boolean }).refresh);
        return snapshot(tasks, extra);
      }
      throw new Error(`unexpected ${cmd}`);
    });
    return reads;
  }

  it('does not start a second read for every click of the refresh button', async () => {
    const reads = signedIn([task()]);
    render(<App />);
    await waitFor(() => expect(screen.getByText('Ship the OAuth flow')).toBeInTheDocument());

    const clock = skipRefreshFloor();
    const refresh = screen.getByTitle('Refresh');
    for (let i = 0; i < 5; i += 1) fireEvent.click(refresh);
    await act(async () => {
      await new Promise((r) => setTimeout(r, 20));
    });
    clock.mockRestore();

    expect(
      reads.filter(Boolean).length,
      'five clicks must not stack five table reads'
    ).toBe(2); // the one at sign-in, and one for the whole burst
  });

  it('does not start a read every time the window regains focus', async () => {
    const reads = signedIn([task()]);
    render(<App />);
    await waitFor(() => expect(screen.getByText('Ship the OAuth flow')).toBeInTheDocument());

    const focused = vi.spyOn(document, 'hasFocus').mockReturnValue(true);
    const clock = skipRefreshFloor();
    await act(async () => {
      for (let i = 0; i < 10; i += 1) window.dispatchEvent(new Event('focus'));
      await new Promise((r) => setTimeout(r, 20));
    });
    clock.mockRestore();
    focused.mockRestore();

    expect(reads.filter(Boolean).length, 'alt-tabbing must not stack reads').toBe(2);
  });
});

describe('the in-flight marker clears on the answer, not on a timer (R6-3)', () => {
  function pendingThenSettled() {
    let settled = false;
    invokeMock.mockImplementation(async (cmd) => {
      if (cmd === 'sign_in') return VIEWER;
      if (cmd === 'update_task') {
        settled = true;
        return snapshot([task({ status: 'Backlog' })], { pending_ids: ['rec1'] });
      }
      if (cmd === 'list_my_tasks') {
        return snapshot([task({ status: settled ? 'Backlog' : 'In Progress' })], {
          pending_ids: settled ? [] : [],
        });
      }
      throw new Error(`unexpected ${cmd}`);
    });
  }

  it('un-dims the row as soon as Rust says the write settled', async () => {
    pendingThenSettled();
    render(<App />);
    await waitFor(() => expect(screen.getByText('Ship the OAuth flow')).toBeInTheDocument());

    fireEvent.click(screen.getByTitle('Back to Backlog'));
    await waitFor(() => expect(document.querySelector('.task--pending')).not.toBeNull());

    // Rust emits the event the moment the PUT settles. No timer involved.
    await act(async () => {
      emitWriteSettled();
      await Promise.resolve();
    });

    expect(
      document.querySelector('.task--pending'),
      'the row is still dimmed after Rust reported the outcome'
    ).toBeNull();
  });

  it('still un-dims the row when the event never arrives', async () => {
    // A missed or duplicated event must not strand a row dimmed forever.
    pendingThenSettled();
    render(<App />);
    await waitFor(() => expect(screen.getByText('Ship the OAuth flow')).toBeInTheDocument());

    fireEvent.click(screen.getByTitle('Back to Backlog'));
    await waitFor(() => expect(document.querySelector('.task--pending')).not.toBeNull());

    await act(async () => {
      await new Promise((r) => setTimeout(r, 1_400));
    });

    expect(document.querySelector('.task--pending')).toBeNull();
  });
});

describe('fixing a typo in a title (R7)', () => {
  function signedInWith(tasks: Task[], onUpdate?: (args: unknown) => Snapshot) {
    invokeMock.mockImplementation(async (cmd, args) => {
      if (cmd === 'sign_in') return VIEWER;
      if (cmd === 'list_my_tasks') return snapshot(tasks);
      if (cmd === 'update_task') {
        if (onUpdate) return onUpdate(args);
        return snapshot(tasks);
      }
      throw new Error(`unexpected ${cmd}`);
    });
  }

  function expand(title: string) {
    fireEvent.click(screen.getByText(title).closest('.task__body') as HTMLElement);
  }

  it('offers EDIT and DELETE only once the row is expanded', async () => {
    signedInWith([task()]);
    render(<App />);
    await waitFor(() => expect(screen.getByText('Ship the OAuth flow')).toBeInTheDocument());

    expect(screen.queryByText('EDIT')).toBeNull();
    expect(screen.queryByText('DELETE')).toBeNull();

    expand('Ship the OAuth flow');

    expect(screen.getByText('EDIT')).toBeInTheDocument();
    expect(screen.getByText('DELETE')).toBeInTheDocument();
    // And the hover strip stays two buttons wide at 360px.
    expect(document.querySelectorAll('.task__actions button')).toHaveLength(2);
  });

  it('sends only the title, so a concurrent plugin edit is not clobbered', async () => {
    const sent: unknown[] = [];
    signedInWith([task()], (args) => {
      sent.push(args);
      return snapshot([task({ title: 'Ship the OAuth flow properly' })]);
    });
    render(<App />);
    await waitFor(() => expect(screen.getByText('Ship the OAuth flow')).toBeInTheDocument());
    expand('Ship the OAuth flow');

    fireEvent.click(screen.getByText('EDIT'));
    const input = screen.getByLabelText('Task title');
    fireEvent.change(input, { target: { value: 'Ship the OAuth flow properly' } });
    fireEvent.keyDown(input, { key: 'Enter' });

    await waitFor(() => expect(sent).toHaveLength(1));
    expect(sent[0]).toEqual({
      recordId: 'rec1',
      patch: { title: 'Ship the OAuth flow properly' },
    });
    await waitFor(() =>
      expect(screen.getByText('Ship the OAuth flow properly')).toBeInTheDocument()
    );
  });

  it('commits from the SAVE action as well as from Enter', async () => {
    const sent: unknown[] = [];
    signedInWith([task()], (args) => {
      sent.push(args);
      return snapshot([task({ title: 'renamed' })]);
    });
    render(<App />);
    await waitFor(() => expect(screen.getByText('Ship the OAuth flow')).toBeInTheDocument());
    expand('Ship the OAuth flow');
    fireEvent.click(screen.getByText('EDIT'));
    fireEvent.change(screen.getByLabelText('Task title'), { target: { value: 'renamed' } });

    fireEvent.click(screen.getByText('SAVE'));

    await waitFor(() => expect(sent).toHaveLength(1));
  });

  it('abandons the edit on Escape and on CANCEL, with nothing sent', async () => {
    const sent: unknown[] = [];
    signedInWith([task()], (args) => {
      sent.push(args);
      return snapshot([task()]);
    });
    render(<App />);
    await waitFor(() => expect(screen.getByText('Ship the OAuth flow')).toBeInTheDocument());
    expand('Ship the OAuth flow');

    fireEvent.click(screen.getByText('EDIT'));
    fireEvent.change(screen.getByLabelText('Task title'), { target: { value: 'typo' } });
    fireEvent.keyDown(screen.getByLabelText('Task title'), { key: 'Escape' });

    expect(screen.queryByLabelText('Task title')).toBeNull();
    expect(screen.getByText('Ship the OAuth flow')).toBeInTheDocument();

    fireEvent.click(screen.getByText('EDIT'));
    fireEvent.change(screen.getByLabelText('Task title'), { target: { value: 'typo again' } });
    fireEvent.click(screen.getByText('CANCEL'));

    expect(screen.queryByLabelText('Task title')).toBeNull();
    expect(sent, 'cancelling must not write').toHaveLength(0);
  });

  it('does not collapse the row while the input has focus', async () => {
    signedInWith([task()]);
    render(<App />);
    await waitFor(() => expect(screen.getByText('Ship the OAuth flow')).toBeInTheDocument());
    expand('Ship the OAuth flow');
    fireEvent.click(screen.getByText('EDIT'));

    const input = screen.getByLabelText('Task title');
    fireEvent.click(input);
    fireEvent.keyDown(input, { key: ' ' });

    expect(
      screen.queryByLabelText('Task title'),
      'the row-expand handler fired and closed the editor'
    ).not.toBeNull();
  });

  it('refuses a blank title, says why, and does not lose the original', async () => {
    const refusal: UiError = {
      kind: 'invalid',
      message: 'A task needs a title.',
      needs_login: false,
    };
    invokeMock.mockImplementation(async (cmd) => {
      if (cmd === 'sign_in') return VIEWER;
      if (cmd === 'list_my_tasks') return snapshot([task()]);
      if (cmd === 'update_task') throw refusal;
      throw new Error(`unexpected ${cmd}`);
    });
    render(<App />);
    await waitFor(() => expect(screen.getByText('Ship the OAuth flow')).toBeInTheDocument());
    expand('Ship the OAuth flow');

    fireEvent.click(screen.getByText('EDIT'));
    fireEvent.change(screen.getByLabelText('Task title'), { target: { value: '   ' } });
    fireEvent.keyDown(screen.getByLabelText('Task title'), { key: 'Enter' });

    await waitFor(() => expect(screen.getByText(/A task needs a title/)).toBeInTheDocument());
    expect(
      screen.getByLabelText('Task title'),
      'the editor must stay open on the refused text'
    ).toBeInTheDocument();

    fireEvent.click(screen.getByText('CANCEL'));
    expect(screen.getByText('Ship the OAuth flow')).toBeInTheDocument();
  });
});

describe('deleting a task (R7)', () => {
  function signedIn(onDelete?: () => Snapshot) {
    const deletes: unknown[] = [];
    invokeMock.mockImplementation(async (cmd, args) => {
      if (cmd === 'sign_in') return VIEWER;
      if (cmd === 'list_my_tasks') return snapshot([task()]);
      if (cmd === 'delete_task') {
        deletes.push(args);
        if (onDelete) return onDelete();
        return snapshot([]);
      }
      throw new Error(`unexpected ${cmd}`);
    });
    return deletes;
  }

  async function openConfirm() {
    render(<App />);
    await waitFor(() => expect(screen.getByText('Ship the OAuth flow')).toBeInTheDocument());
    fireEvent.click(screen.getByText('Ship the OAuth flow').closest('.task__body') as HTMLElement);
    fireEvent.click(screen.getByText('DELETE'));
    return document.querySelector('.confirm') as HTMLElement;
  }

  it('asks first, and says the deletion cannot be undone', async () => {
    const deletes = signedIn();
    const confirm = await openConfirm();

    expect(confirm).not.toBeNull();
    expect(screen.getByText(/cannot be undone/i)).toBeInTheDocument();
    expect(confirm.textContent, 'the confirm must name the task').toContain(
      'Ship the OAuth flow'
    );
    expect(screen.getAllByText('Ship the OAuth flow').length).toBeGreaterThan(0);
    expect(deletes, 'nothing may be deleted before the confirm is answered').toHaveLength(0);
  });

  it('does not dress the destructive button up as the safe one', async () => {
    // Muscle memory from MARK AS DONE? -> YES must not destroy a record.
    signedIn();
    const confirm = await openConfirm();
    const destructive = Array.from(confirm.querySelectorAll('button')).find(
      (b) => b.textContent === 'DELETE'
    ) as HTMLButtonElement;

    expect(destructive.className).toContain('pixel-btn--danger');
    expect(destructive.className).not.toContain('pixel-btn--ok');
    expect(confirm.textContent).not.toContain('YES');
    expect(document.activeElement?.textContent, 'focus must start on the way out').toBe(
      'CANCEL'
    );
  });

  it('cancels from the backdrop and from Escape', async () => {
    const deletes = signedIn();
    const confirm = await openConfirm();

    fireEvent.click(confirm);
    expect(document.querySelector('.confirm')).toBeNull();

    fireEvent.click(screen.getByText('DELETE'));
    expect(document.querySelector('.confirm')).not.toBeNull();
    fireEvent.keyDown(window, { key: 'Escape' });
    await waitFor(() => expect(document.querySelector('.confirm')).toBeNull());

    expect(deletes).toHaveLength(0);
    expect(screen.getByText('Ship the OAuth flow')).toBeInTheDocument();
  });

  it('removes the row once the Base has confirmed it', async () => {
    const deletes = signedIn();
    const confirm = await openConfirm();

    fireEvent.click(
      Array.from(confirm.querySelectorAll('button')).find(
        (b) => b.textContent === 'DELETE'
      ) as HTMLButtonElement
    );

    await waitFor(() => expect(deletes).toHaveLength(1));
    expect(deletes[0]).toEqual({ recordId: 'rec1' });
    await waitFor(() => expect(screen.queryByText('Ship the OAuth flow')).toBeNull());
    expect(screen.getByText(/NOTHING ASSIGNED/i)).toBeInTheDocument();
  });

  it('puts the row back and says why when the Base refuses', async () => {
    const refusal: UiError = {
      kind: 'forbidden',
      message: 'You do not have permission to do that in this Base.',
      needs_login: false,
    };
    invokeMock.mockImplementation(async (cmd) => {
      if (cmd === 'sign_in') return VIEWER;
      if (cmd === 'list_my_tasks') return snapshot([task()]);
      if (cmd === 'delete_task') throw refusal;
      throw new Error(`unexpected ${cmd}`);
    });
    const confirm = await openConfirm();

    fireEvent.click(
      Array.from(confirm.querySelectorAll('button')).find(
        (b) => b.textContent === 'DELETE'
      ) as HTMLButtonElement
    );

    await waitFor(() => expect(screen.getByText(/do not have permission/)).toBeInTheDocument());
    expect(
      screen.getByText('Ship the OAuth flow'),
      'a refused delete must not leave the row vanished'
    ).toBeInTheDocument();
  });
});
