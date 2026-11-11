import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import vm from 'node:vm';

const scope = {};
vm.runInNewContext(readFileSync(new URL('../try-field.js', import.meta.url), 'utf8'), scope);
function fixture(convert, configVersion) {
  function element(value = '') {
    const handlers = {};
    return { value, handlers, selectionStart: value.length, selectionEnd: value.length,
      addEventListener(name, fn) { (handlers[name] ??= []).push(fn); },
      setSelectionRange(a, b) { this.selectionStart = a; this.selectionEnd = b; },
      async emit(name, event = {}) { for (const fn of handlers[name] ?? []) await fn(event); },
    };
  }
  const input = element('ghbdtn '), layout = element('us'), results = [];
  input.ownerDocument = { activeElement: input };
  scope.RekeyTryField({ input, layout, convert, configVersion, onResult: r => results.push(r) });
  return { input, layout, results };
}
const fixed = { changed: true, output: 'привет ' };
test('word boundary corrects the practice field itself and keeps following text', async () => {
  const { input, results } = fixture(async ({ text }) => { assert.equal(text, 'ghbdtn '); return fixed; });
  input.value += 'tail'; input.setSelectionRange(7, 7);
  await input.emit('input', { inputType: 'insertText' });
  assert.equal(input.value, 'привет tail');
  assert.equal(input.selectionStart, 7);
  assert.equal(results.length, 1);
  let prevented = false;
  await input.emit('keydown', { key: 'z', ctrlKey: true, preventDefault() { prevented = true; } });
  assert.equal(input.value, 'ghbdtn tail');
  assert.equal(prevented, true);
});
test('typing, caret movement, blur, layout changes and IME invalidate pending responses', async () => {
  for (const change of [
    async f => { f.input.value += 'x'; f.input.setSelectionRange(8,8); await f.input.emit('input'); },
    async f => { f.input.setSelectionRange(1,1); await f.input.emit('select'); },
    async f => { f.input.ownerDocument.activeElement = null; await f.input.emit('blur'); },
    async f => { f.layout.value = 'ru'; await f.layout.emit('change'); },
    async f => f.input.emit('compositionstart'),
  ]) {
    let release;
    const f = fixture(() => new Promise(resolve => { release = resolve; }));
    const pending = f.input.emit('input');
    await change(f); const expected = f.input.value;
    release(fixed); await pending;
    assert.equal(f.input.value, expected); assert.equal(f.results.length, 0);
  }
});
test('exceptions, backend errors, selection, deletion and active composition keep typed input', async () => {
  for (const convert of [async () => ({ changed: false, output: 'ghbdtn ', exception: true }),
    async () => { throw Error('offline'); }]) {
    const f = fixture(convert); await f.input.emit('input'); assert.equal(f.input.value, 'ghbdtn ');
  }
  for (const prepare of [
    async f => f.input.setSelectionRange(0,7),
    async f => f.input.emit('compositionstart'),
  ]) {
    const f = fixture(() => { assert.fail('must not convert'); });
    await prepare(f); await f.input.emit('input');
  }
  const f = fixture(() => { assert.fail('must not convert deletion'); });
  await f.input.emit('input', { inputType: 'deleteContentBackward' });
});
test('pausing or changing rules while IPC is pending cannot apply an older correction', async () => {
  let version = 1, release;
  const f = fixture(() => new Promise(resolve => { release = resolve; }), () => version);
  const pending = f.input.emit('input'); version++;
  release(fixed); await pending;
  assert.equal(f.input.value, 'ghbdtn ');
  assert.equal(f.results.length, 0);
});
