# Implementation Plan: Project Settings Update Queue

## Problem Summary

### Concurrent Settings Update Race Conditions

When a user rapidly changes multiple settings in the Settings UI, the current implementation spawns a new async task for each change. Each task independently:

1. Reads the current buffer content (`buffer.text()`)
2. Computes the new settings JSON by applying its update function
3. Replaces the entire buffer content with the computed result
4. Saves the buffer to disk

This creates a classic read-modify-write race condition. Consider this scenario:

```
Time    Task A (toggle setting X)         Task B (toggle setting Y)
────    ─────────────────────────         ─────────────────────────
T1      Read buffer: {X: false, Y: false}
T2                                        Read buffer: {X: false, Y: false}
T3      Compute: {X: true, Y: false}
T4                                        Compute: {X: false, Y: true}
T5      Write buffer: {X: true, Y: false}
T6                                        Write buffer: {X: false, Y: true}  ← OVERWRITES X!
```

At T6, Task B's write overwrites Task A's change because Task B computed its update based on stale content (read at T2, before Task A wrote at T5).

### Current Code Location

The problematic code is in `crates/settings_ui/src/settings_ui.rs` in the `update_project_setting_file` function, which spawns an independent async task via `cx.spawn(...)` without any coordination with other concurrent updates.

### Symptoms

- Rapidly toggling multiple settings may cause some settings to revert to their previous values
- The more settings changed in quick succession, the higher the likelihood of data loss
- Users may not notice immediately, leading to confusion about why settings "don't stick"

## Proposed Solution: Global Update Queue

Following the existing pattern in `SettingsStore` (which uses `setting_file_updates_tx` channel), we'll implement a serialized queue for project settings updates.

---

## Architecture Overview

The `SettingsStore` already serializes user settings updates via a channel:

```rust
setting_file_updates_tx:
    mpsc::UnboundedSender<Box<dyn FnOnce(AsyncApp) -> LocalBoxFuture<'static, Result<()>>>>,
```

We'll create an analogous mechanism for project settings.

---

## Implementation Steps

### Step 1: Define the Update Entry Struct

Create a struct that captures all necessary context for a project settings update:

```rust
// New struct to hold all update context
struct ProjectSettingsUpdateEntry {
    worktree_id: WorktreeId,
    rel_path: Arc<RelPath>,
    settings_window: WeakEntity<SettingsWindow>,
    project: WeakEntity<Project>,
    worktree: Entity<Worktree>,
    needs_creation: bool,
    update: Rc<dyn Fn(&mut SettingsContent, &App)>,
}
```

**Why `Rc<dyn Fn>`?**
- `Rc` for local-thread usage without `Send` requirement
- `Fn` trait allows straightforward invocation
- No `Send` requirement since we use an entity-based queue (all operations on main thread)

### Step 2: Create the Queue Global

Two design options:

#### Option A: Entity-based Queue (Recommended for `Rc` support)

Since GPUI entities are accessed only from the main thread, we can use `Rc` freely:

```rust
struct ProjectSettingsUpdateQueue {
    pending: VecDeque<ProjectSettingsUpdateEntry>,
    processing_task: Option<Task<()>>,
}

impl Global for ProjectSettingsUpdateQueue {}

impl ProjectSettingsUpdateQueue {
    fn new() -> Self {
        Self {
            pending: VecDeque::new(),
            processing_task: None,
        }
    }

    fn enqueue(cx: &mut App, entry: ProjectSettingsUpdateEntry) {
        cx.update_global::<Self, _>(|queue, cx| {
            queue.pending.push_back(entry);
            queue.process_next_if_idle(cx);
        });
    }

    fn process_next_if_idle(&mut self, cx: &mut App) {
        if self.processing_task.is_some() {
            return; // Already processing
        }

        let Some(entry) = self.pending.pop_front() else {
            return; // Nothing to process
        };

        self.processing_task = Some(cx.spawn(async move |cx| {
            let result = Self::process_entry(entry, cx).await;

            cx.update_global::<Self, _>(|queue, cx| {
                queue.processing_task = None;
                queue.process_next_if_idle(cx);
            })?;

            result
        }));
    }
}
```

#### Option B: Channel-based Queue (Follows `SettingsStore` pattern exactly)

If we follow the `SettingsStore` pattern with channels, we'd use `Box<dyn FnOnce + Send>` instead of `Rc`:

```rust
struct ProjectSettingsUpdateQueue {
    sender: mpsc::UnboundedSender<ProjectSettingsUpdateEntry>,
    _processor: Task<()>,
}

// Entry would need Send-compatible types:
struct ProjectSettingsUpdateEntry {
    worktree_id: WorktreeId,
    rel_path: Arc<RelPath>,
    settings_window: WeakEntity<SettingsWindow>,
    project: WeakEntity<Project>,
    worktree: WeakEntity<Worktree>,
    needs_creation: bool,
    update: Box<dyn FnOnce(&mut SettingsContent, &App) + Send>,
}
```

**Recommendation**: Option A (Entity-based) since the user specifically requested `Rc<impl Fn>`.

### Step 3: Initialize the Queue

Add initialization to the module init or `SettingsWindow::new`:

```rust
// In init function or SettingsWindow::new
pub fn init(cx: &mut App) {
    // ... existing init code ...

    if !cx.has_global::<ProjectSettingsUpdateQueue>() {
        cx.set_global(ProjectSettingsUpdateQueue::new());
    }
}
```

### Step 4: Implement the Entry Processor

Move the current async logic from `update_project_setting_file` into the queue processor:

```rust
impl ProjectSettingsUpdateQueue {
    async fn process_entry(
        entry: ProjectSettingsUpdateEntry,
        cx: AsyncApp,
    ) -> Result<()> {
        let ProjectSettingsUpdateEntry {
            worktree_id,
            rel_path,
            settings_window,
            project,
            worktree,
            needs_creation,
            update,
        } = entry;

        let project_path = ProjectPath {
            worktree_id,
            path: rel_path.clone(),
        };

        // Create file if needed
        if needs_creation {
            worktree.update(&cx, |worktree, cx| {
                worktree.create_entry(rel_path.clone(), false, None, cx)
            })?.await?;
        }

        // Get or reload buffer (existing logic)
        let cached_buffer = settings_window.read_with(&cx, |sw, _| {
            sw.project_setting_file_buffers.get(&project_path).cloned()
        })?;

        let buffer = if let Some(cached) = cached_buffer {
            // ... reload if has_conflict ...
            cached
        } else {
            // ... open buffer, cache it ...
        };

        // Apply update
        buffer.update(&cx, |buffer, cx| {
            let current_text = buffer.text();
            let new_text = cx.global::<SettingsStore>()
                .new_text_for_update(current_text, |settings| (update)(settings, cx));
            buffer.edit([(0..buffer.len(), new_text)], None, cx);
        })?;

        // Save buffer
        // ... existing save logic ...

        Ok(())
    }
}
```

### Step 5: Modify `update_project_setting_file`

Replace the spawn-based approach with queue submission:

```rust
fn update_project_setting_file(
    worktree_id: WorktreeId,
    rel_path: Arc<RelPath>,
    update: impl 'static + FnOnce(&mut SettingsContent, &App),
    settings_window: Entity<SettingsWindow>,
    cx: &mut App,
) -> Result<()> {
    let Some((worktree, project)) = all_projects(
        settings_window.read(cx).original_window.as_ref(),
        cx
    ).find_map(|project| {
        project.read(cx).worktree_for_id(worktree_id, cx).zip(Some(project))
    }) else {
        anyhow::bail!("Could not find project with worktree id: {}", worktree_id);
    };

    let rel_path = rel_path.join(paths::local_settings_file_relative_path());

    let needs_creation = !project
        .read(cx)
        .contains_local_settings_file(worktree_id, &rel_path, cx);

    let entry = ProjectSettingsUpdateEntry {
        worktree_id,
        rel_path,
        settings_window: settings_window.downgrade(),
        project: project.downgrade(),
        worktree,
        needs_creation,
        update: Rc::new(update),
    };

    ProjectSettingsUpdateQueue::enqueue(cx, entry);

    Ok(())
}
```

---

## Key Design Decisions

| Decision | Rationale |
|----------|-----------|
| **Entity-based queue over channel** | Allows `Rc` usage as user requested; all operations stay on main thread |
| **`WeakEntity` for window/project** | Avoids keeping entities alive if they close before update processes |
| **`Rc<dyn Fn>` for update** | Simple callable wrapper; no `Send` requirement since queue is main-thread only |
| **Single global queue** | Simple; could be extended to per-file queues for parallelism if needed |
| **Process-next-on-completion pattern** | Ensures serialization without blocking |

---

## Files to Modify

1. **`crates/settings_ui/src/settings_ui.rs`**:
   - Add `ProjectSettingsUpdateQueue` struct
   - Add `ProjectSettingsUpdateEntry` struct
   - Add `impl Global for ProjectSettingsUpdateQueue`
   - Add queue initialization in module init
   - Modify `update_project_setting_file` to submit to queue instead of spawning

---

## Testing Strategy

1. **Unit test**: Mock rapid sequential updates, verify all are applied
2. **Integration test**: Rapidly toggle multiple settings, verify final state reflects all changes
3. **Edge case**: Queue update while previous is processing, verify serialization
