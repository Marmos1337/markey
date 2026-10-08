// Serialize settings writes and derive each patch from a fresh backend snapshot.
// A failed save does not poison later requests; no optimistic state is persisted.
globalThis.RekeyConfigWriter = function ({ readState, writeState, toConfig, onState }) {
  let tail = Promise.resolve();
  return {
    enqueue(changes = {}, extras = {}) {
      const task = tail.then(async () => {
        const fresh = await readState();
        const patch = typeof changes === "function" ? changes(fresh) : changes;
        await writeState({ ...toConfig(fresh), ...patch }, extras);
        onState(await readState());
      });
      tail = task.catch(() => {});
      return task;
    },
  };
};
