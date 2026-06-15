# Ticket: TUI Search/Filtering Mode Navigation & Hotkey Lockout

**Status**: Open
**Component**: TUI Control Room (`src/controlroom.rs`, `src/picker.rs`)
**Severity**: High (Core Usability Defect)

---

## 1. Problem Description
In the TUI interface, entering filtering mode by pressing `/` locks the user into a text-input state. While in this state, character key presses are appended directly to the filter query (`st.query`). 

However, there is currently **no way to exit the text-input state while keeping the search filter active**. This creates a major usability bug:
* If the user presses `Esc` to stop typing, the TUI exits filtering mode but **clears the search query entirely** (`query.clear()`).
* If the user presses `Enter`, it **immediately executes the default action** (e.g. activates the currently selected agent, or selects the item in a picker), rather than submitting/locking in the text filter.
* Because the user is trapped in the text-input state while the filter is active, they **cannot use any hotkeys** on the filtered list items (such as `.` to toggle danger, `k` to kill a session, or `t` to open tools) because pressing any of these keys simply types them into the search input.

---

## 2. Root Cause Analysis

### A. In `src/controlroom.rs`
The key handler for filtering mode is implemented in `on_key` around line 1794:
```rust
    // Filter editing.
    if st.filtering {
        match code {
            KeyCode::Esc => { st.filtering = false; st.query.clear(); st.sel[st.tab] = 0; }
            KeyCode::Backspace => { st.query.pop(); st.sel[st.tab] = 0; }
            KeyCode::Char(c) => { st.query.push(c); st.sel[st.tab] = 0; }
            KeyCode::Enter => return Some(activate(st, running, run_idx, sessions, sess_idx, false)),
            KeyCode::Up | KeyCode::Down => {} // fallthrough below
            _ => return Some(Flow::Continue),
        }
        if !matches!(code, KeyCode::Up | KeyCode::Down) {
            return Some(Flow::Continue);
        }
    }
```
* `Esc` resets everything and clears the filter.
* `Enter` immediately runs `activate`.
* There is no key transition to turn off `st.filtering` while preserving `st.query`.

### B. In `src/picker.rs`
A similar pattern is implemented around line 154:
```rust
                if filtering {
                    match key.code {
                        KeyCode::Esc => {
                            filtering = false;
                            query.clear();
                            selected = 0;
                            continue;
                        }
                        // ...
```

---

## 3. Proposed Solution

### Retain Filter Query on Focus Loss
We should split the state into `editing_filter` (whether the search box is focused and accepting text) and `has_active_filter` (whether the list is currently filtered by the query):
1. Pressing `/` enters search editing mode (`filtering = true`).
2. Pressing `Enter` in search editing mode **submits/locks** the query and exits the text-input focus (`filtering = false`), but **leaves the query intact** so the list remains filtered.
3. Once focus is returned to the list, the user can navigate the filtered items using arrows/j/k and press hotkeys (like `k` to kill, `.` to toggle, `t` to open tools, or `Enter` to activate the highlighted item).
4. Pressing `Esc` while the filter is active should clear the query and restore the full list.
