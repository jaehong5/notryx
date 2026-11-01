import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import vm from 'node:vm';

const script = readFileSync(new URL('../third-party/drawio/bridge.js', import.meta.url), 'utf8');

function setup() {
  const listeners = new Map();
  const sent = [];
  const received = [];
  let starts = 0;
  const window = {
    parent: { postMessage: (...message) => sent.push(message) },
    checkAllLoaded: () => { starts++; },
    mxScriptsLoaded: false,
    mxWinLoaded: false,
    addEventListener: (type, listener) => listeners.set(type, listener),
    removeEventListener: type => listeners.delete(type),
    dispatchEvent: event => received.push(event),
  };
  const Editor = {};
  const EditorUi = { prototype: { installMessageHandler() {} } };
  vm.runInNewContext(script, {
    window, Editor, EditorUi, TextEncoder,
    MessageEvent: class { constructor(type, values) { Object.assign(this, { type }, values); } },
  });
  const transmit = event => listeners.get('message')?.(event);
  return { window, EditorUi, sent, received, transmit, starts: () => starts };
}

test('waits for one authorized parent port and complete script loading', () => {
  const state = setup();
  assert.equal(state.sent[0][0].event, 'notryx-drawio-ready');
  const outbound = [];
  const port = { postMessage: value => outbound.push(value), start() {} };
  const bind = {
    source: state.window.parent, origin: 'tauri://localhost', ports: [port],
    data: { event: 'notryx-drawio-bind' },
  };
  state.transmit({ ...bind, source: {} });
  state.transmit({ ...bind, origin: 'https://example.invalid' });
  state.window.mxScriptsLoaded = true;
  state.window.mxWinLoaded = true;
  state.window.checkAllLoaded();
  assert.equal(state.starts(), 0);
  state.transmit(bind);
  assert.equal(state.starts(), 1);
  assert.equal(state.EditorUi.prototype.embedMessageSource, port);
  port.postMessage('{"event":"init"}', '*');
  assert.deepEqual(outbound, ['{"event":"init"}']);
  port.onmessage({ data: '{"action":"load","xml":"synthetic"}' });
  assert.equal(state.received[0].source, port);
  assert.equal(state.received[0].data, '{"action":"load","xml":"synthetic"}');
  state.transmit({ ...bind, ports: [{}] });
  state.window.checkAllLoaded();
  assert.equal(state.starts(), 1);
});

test('forwards only bounded autosave XML and reports unsupported draft sizes', () => {
  const state = setup();
  const outbound = [];
  const port = { postMessage: value => outbound.push(value), start() {} };
  state.transmit({
    source: state.window.parent, origin: 'tauri://localhost', ports: [port],
    data: { event: 'notryx-drawio-bind' },
  });
  const xml = '<mxfile>' + ' '.repeat(6000) + '</mxfile>';
  port.postMessage(JSON.stringify({ event: 'autosave', xml, message: { action: 'load', xml: 'original' } }), '*');
  assert.equal(outbound.pop(), JSON.stringify({ event: 'autosave', xml }));
  for (const value of [undefined, '', 1, '한'.repeat(2 * 1024 * 1024)]) {
    port.postMessage(JSON.stringify({ event: 'autosave', xml: value }), '*');
    assert.equal(outbound.pop(), '{"event":"draft-error"}');
  }
});

test('port binding before application load does not start the application early', () => {
  const state = setup();
  state.transmit({
    source: state.window.parent, origin: 'http://tauri.localhost',
    ports: [{ postMessage() {}, start() {} }], data: { event: 'notryx-drawio-bind' },
  });
  assert.equal(state.starts(), 0);
  state.window.mxScriptsLoaded = true;
  state.window.checkAllLoaded();
  assert.equal(state.starts(), 0);
  state.window.mxWinLoaded = true;
  state.window.checkAllLoaded();
  assert.equal(state.starts(), 1);
});

test('projects upstream responses without echoing diagram XML or unsupported actions', () => {
  const state = setup();
  const outbound = [];
  const port = { postMessage: value => outbound.push(value), start() {} };
  state.transmit({
    source: state.window.parent, origin: 'tauri://localhost', ports: [port],
    data: { event: 'notryx-drawio-bind' },
  });
  for (const event of ['init', 'load', 'exit']) {
    port.postMessage(JSON.stringify({ event, xml: '<mxfile>' + ' '.repeat(6000) + '</mxfile>' }), '*');
  }
  for (const data of [null, {}, 'null', '{', JSON.stringify({ event: 'save', xml: '<mxfile/>' }),
    JSON.stringify({ event: 'openLink', href: 'https://example.invalid' })]) {
    port.postMessage(data, '*');
  }
  assert.deepEqual(outbound, ['{"event":"init"}', '{"event":"load"}', '{"event":"exit"}']);
});

function editorSetup(exportCanvas) {
  const state = setup();
  const outbound = [];
  const port = { postMessage: value => outbound.push(JSON.parse(value)), start() {} };
  state.window.mxScriptsLoaded = state.window.mxWinLoaded = true;
  state.transmit({ source: state.window.parent, origin: 'tauri://localhost', ports: [port],
    data: { event: 'notryx-drawio-bind' } });
  let stopped = false;
  const ui = {
    getFileData: () => '<mxfile/>',
    spinner: { stop() {} },
    editor: {
      graph: { stopEditing(cancel) { assert.equal(cancel, false); stopped = true; }, setEnabled() {} },
      isExportToCanvas: () => true,
      exportToCanvas: exportCanvas ?? ((...args) => { args[4](new Error('canvas failed')); }),
    },
  };
  state.EditorUi.prototype.installMessageHandler.call(ui);
  const request = data => port.onmessage({ data: JSON.stringify(data) });
  return { ...state, outbound, port, ui, request, stopped: () => stopped };
}

test('snapshot commits active label editing and PNG export preserves the existing model', () => {
  const state = editorSetup();
  state.request({ action: 'snapshot', requestId: 'snapshot-1' });
  assert.equal(state.stopped(), true);
  assert.deepEqual(state.outbound.pop(), { event: 'snapshot', requestId: 'snapshot-1', xml: '<mxfile/>' });
  state.request({ action: 'export', format: 'xmlpng', requestId: 'png-1', xml: '<mxfile/>' });
  assert.deepEqual(JSON.parse(state.received.at(-1).data), { action: 'export', format: 'xmlpng', requestId: 'png-1' });
  state.port.postMessage(JSON.stringify({ event: 'export', format: 'xmlpng', message: { requestId: 'old' }, data: 'data:image/png;base64,aGVsbG8=' }));
  assert.equal(state.outbound.length, 0);
  state.port.postMessage(JSON.stringify({ event: 'export', format: 'xmlpng', message: { requestId: 'png-1' }, data: 'data:image/png;base64,aGVsbG8=', xml: '<mxfile/>' }));
  assert.deepEqual(state.outbound.pop(), { event: 'export', requestId: 'png-1', data: 'data:image/png;base64,aGVsbG8=' });
});

test('rejects changed snapshots, unavailable canvas and unsupported parent actions', () => {
  const state = editorSetup();
  state.request({ action: 'export', format: 'xmlpng', requestId: 'png-1', xml: '<changed/>' });
  assert.deepEqual(state.outbound.pop(), { event: 'export-error', requestId: 'png-1' });
  state.ui.editor.isExportToCanvas = () => false;
  state.request({ action: 'export', format: 'xmlpng', requestId: 'png-2', xml: '<mxfile/>' });
  assert.deepEqual(state.outbound.pop(), { event: 'export-error', requestId: 'png-2' });
  state.request({ action: 'open', url: 'https://example.invalid' });
  assert.equal(state.received.length, 0);
});

test('canvas failure reports export failure and suppresses upstream blank PNG success', () => {
  const state = editorSetup();
  state.request({ action: 'export', format: 'xmlpng', requestId: 'png-1', xml: '<mxfile/>' });
  let cleanup = false;
  state.ui.editor.exportToCanvas(null, null, null, null, () => { cleanup = true; });
  assert.equal(cleanup, true);
  assert.deepEqual(state.outbound.pop(), { event: 'export-error', requestId: 'png-1' });
  state.port.postMessage(JSON.stringify({ event: 'export', format: 'xmlpng', message: { requestId: 'png-1' }, data: 'data:image/png;base64,aGVsbG8=' }));
  assert.equal(state.outbound.length, 0);
  state.request({ action: 'cancel-export' });
  state.request({ action: 'snapshot', requestId: 'snapshot-2' });
  assert.equal(state.outbound.pop().event, 'snapshot');
});


test('asynchronous canvas serialization failures use the error path instead of escaping', async () => {
  let success;
  let cleanup = false;
  const state = editorSetup(callback => { success = callback; });
  state.request({ action: 'export', format: 'xmlpng', requestId: 'png-async', xml: '<mxfile/>' });
  state.ui.editor.exportToCanvas(canvas => canvas.toDataURL(), null, null, null, () => { cleanup = true; });
  await Promise.resolve();
  assert.doesNotThrow(() => success({ toDataURL() { throw new Error('serialization failed'); } }));
  assert.equal(cleanup, true);
  assert.deepEqual(state.outbound.pop(), { event: 'export-error', requestId: 'png-async' });
  state.port.postMessage(JSON.stringify({ event: 'export', format: 'xmlpng', message: { requestId: 'png-async' }, data: 'data:image/png;base64,aGVsbG8=' }));
  assert.equal(state.outbound.length, 0);
});
