(() => {
  let port = null;
  let started = false;
  const upstreamStart = window.checkAllLoaded;

  // The sandbox has an opaque origin. Diagram data only crosses the bound port.
  window.checkAllLoaded = () => {
    if (!started && port && window.mxScriptsLoaded && window.mxWinLoaded) {
      started = true;
      window.PROXY_URL = null;
      window.EXPORT_URL = null;
      window.ICONSEARCH_PATH = null;
      window.ICON_SERVICE_PATH = null;
      Editor.enableServiceWorker = false;
      EditorUi.enableLogging = false;
      EditorUi.prototype.embedMessageSource = port;
      upstreamStart();
    }
  };

  window.addEventListener('message', function bind(event) {
    if (port || event.source !== window.parent || event.ports.length !== 1 ||
      event.data?.event !== 'notryx-drawio-bind') return;
    if (!['tauri://localhost', 'http://tauri.localhost', 'http://127.0.0.1:1420'].includes(event.origin)) return;
    port = event.ports[0];
    const send = port.postMessage.bind(port);
    // Upstream load responses echo XML; only bounded control events cross to the host.
    port.postMessage = data => {
      if (typeof data !== 'string') return;
      try {
        const value = JSON.parse(data);
        if (value && ['init', 'load', 'exit'].includes(value.event)) {
          send(JSON.stringify({ event: value.event }));
        }
      } catch { return; }
    };
    port.onmessage = message => {
      window.dispatchEvent(new MessageEvent('message', {
        data: message.data, source: port
      }));
    };
    port.start();
    window.removeEventListener('message', bind);
    window.checkAllLoaded();
  });
  window.parent.postMessage({ event: 'notryx-drawio-ready' }, '*');
})();
