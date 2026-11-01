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
  const EditorUi = { prototype: {} };
  vm.runInNewContext(script, {
    window, Editor, EditorUi,
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
