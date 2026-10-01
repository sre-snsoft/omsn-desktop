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
    created: Date.now(),
    modified: Date.now(),
    ...over,
  };
}

function snapshot(tasks: Task[] = []): Snapshot {
  return { tasks, fetched_at_millis: Date.now(), pending_ids: [], stale: false };
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

beforeEach(() => {
  invokeMock.mockReset();
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
