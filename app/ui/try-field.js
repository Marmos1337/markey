// The dedicated practice editor stays local. Global hooks deliberately ignore
// settings windows, including passwords and the exception-list editor.
globalThis.RekeyTryField = function ({ input, layout, convert, onResult, configVersion = () => 0 }) {
  let revision = 0;
  let composing = false;
  let undo = null;
  const invalidate = () => { revision++; };
  input.addEventListener("compositionstart", () => { composing = true; invalidate(); });
  input.addEventListener("compositionend", () => { composing = false; invalidate(); });
  input.addEventListener("blur", invalidate);
  input.addEventListener("select", invalidate);
  layout.addEventListener("change", invalidate);
  input.addEventListener("keydown", (event) => {
    // Keep the editor's last automatic replacement reversible without changing
    // the global learned-exception list.
    if ((event.ctrlKey || event.metaKey) && !event.shiftKey && event.key.toLowerCase() === "z"
        && undo && input.value === undo.after) {
      event.preventDefault();
      input.value = undo.before;
      input.setSelectionRange(undo.caret, undo.caret);
      onResult({ output: undo.before, changed: false, exception: false });
      undo = null;
      invalidate();
    } else if (["ArrowLeft", "ArrowRight", "ArrowUp", "ArrowDown", "Home", "End"].includes(event.key)) {
      invalidate();
    }
  });
  input.addEventListener("input", async (event) => {
    const ticket = ++revision;
    if (composing || event.isComposing || input.readOnly || input.disabled) return;
    if (event.inputType && !event.inputType.startsWith("insert")) return;
    const before = input.value;
    const caret = input.selectionStart;
    const selectedLayout = layout.value;
    const settingsVersion = configVersion();
    if (!selectedLayout || caret !== input.selectionEnd || !/\s$/u.test(before.slice(0, caret))) return;
    try {
      const result = await convert({ text: before.slice(0, caret), layout: selectedLayout });
      if (ticket !== revision || settingsVersion !== configVersion()
          || input.value !== before || layout.value !== selectedLayout
          || input.selectionStart !== caret || input.selectionEnd !== caret
          || input.ownerDocument.activeElement !== input) return;
      if (result.changed) {
        input.value = result.output + before.slice(caret);
        input.setSelectionRange(result.output.length, result.output.length);
        undo = { before, after: input.value, caret };
      }
      onResult(result);
    } catch {
      // An unavailable backend must not eat or replace the user's input.
    }
  });
};
