// Serialize settings writes and derive each patch from a fresh backend snapshot.
// A failed save does not poison later requests; no optimistic state is persisted.
globalThis.RekeyConfigWriter = function ({ readState, writeState, toConfig, onState }) {
  let tail = Promise.resolve();
  return {
    enqueue(changes = {}, extras = {}) {
      const task = tail.then(async () => {
        // Something else can change the settings between the read and the
        // write. Rust refuses such a write, and the change is made again on
        // what is there now.
        for (let attempt = 0; ; attempt++) {
          const fresh = await readState();
          const patch = typeof changes === "function" ? changes(fresh) : changes;
          try {
            await writeState({ ...toConfig(fresh), ...patch }, extras, fresh);
          } catch (error) {
            if (error === "stale-config" && attempt < 2) continue;
            throw error;
          }
          onState(await readState());
          return;
        }
      });
      tail = task.catch(() => {});
      return task;
    },
    // Take a change made elsewhere. It waits its turn: read too early, it would
    // arrive after a queued write has finished and put the old state back.
    reload() {
      const task = tail.then(async () => onState(await readState()));
      tail = task.catch(() => {});
      return task;
    },
  };
};
