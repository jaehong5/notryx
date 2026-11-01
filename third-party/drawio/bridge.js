(() => {
  let port = null;
  let started = false;
  let ui = null;
  let exporting = null;
  let send = null;
  const upstreamStart = window.checkAllLoaded;
  const validXml = xml => typeof xml === 'string' && xml.length > 0 &&
    new TextEncoder().encode(xml).length <= 5 * 1024 * 1024;
  const validId = id => typeof id === 'string' && /^[a-zA-Z0-9-]{1,64}$/.test(id);
  const exportError = requestId => {
    if (exporting?.requestId === requestId) exporting.failed = true;
    send(JSON.stringify({ event: 'export-error', requestId }));
  };

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
      const install = EditorUi.prototype.installMessageHandler;
      EditorUi.prototype.installMessageHandler = function(...args) {
        ui = this;
        const exportCanvas = this.editor.exportToCanvas;
        this.editor.exportToCanvas = function(...canvasArgs) {
          const current = exporting;
          if (current) {
            const error = canvasArgs[4];
            canvasArgs[4] = (...errorArgs) => {
              // Upstream substitutes a blank PNG on failure; never publish it as success.
              exportError(current.requestId);
              return error?.(...errorArgs);
            };
            const success = canvasArgs[0];
            canvasArgs[0] = (...successArgs) => {
              // WebKit invokes this callback from a timer outside upstream's try/catch.
              try { return success(...successArgs); }
              catch (error) { return canvasArgs[4](error); }
            };
          }
          return exportCanvas.apply(this, canvasArgs);
        };
        return install.apply(this, args);
      };
      upstreamStart();
    }
  };

  window.addEventListener('message', function bind(event) {
    if (port || event.source !== window.parent || event.ports.length !== 1 ||
      event.data?.event !== 'notryx-drawio-bind') return;
    if (!['tauri://localhost', 'http://tauri.localhost', 'http://127.0.0.1:1420'].includes(event.origin)) return;
    port = event.ports[0];
    send = port.postMessage.bind(port);
    // Project upstream messages without echoed load XML or arbitrary actions.
    port.postMessage = data => {
      if (typeof data !== 'string') return;
      if (data.length > 80 * 1024 * 1024) {
        if (exporting) exportError(exporting.requestId);
        else send('{"event":"draft-error"}');
        return;
      }
      try {
        const value = JSON.parse(data);
        if (value && ['init', 'load', 'exit'].includes(value.event)) {
          send(JSON.stringify({ event: value.event }));
        } else if (value?.event === 'autosave') {
          send(validXml(value.xml) ? JSON.stringify({ event: 'autosave', xml: value.xml }) : '{"event":"draft-error"}');
        } else if (value?.event === 'export' && exporting && value.message?.requestId === exporting.requestId) {
          const current = exporting;
          exporting = null;
          if (current.failed) return;
          const png = value.data;
          if (value.format !== 'xmlpng' || typeof png !== 'string' ||
            !png.startsWith('data:image/png;base64,') || png.length > 22 + 4 * Math.ceil(25 * 1024 * 1024 / 3)) {
            exportError(current.requestId);
          } else send(JSON.stringify({ event: 'export', requestId: current.requestId, data: png }));
        }
      } catch { return; }
    };
    port.onmessage = message => {
      let data;
      try { data = JSON.parse(message.data); } catch { return; }
      if (data?.action === 'snapshot' && validId(data.requestId)) {
        try {
          ui.editor.graph.stopEditing(false);
          const xml = ui.getFileData(true);
          if (!validXml(xml)) throw new Error('invalid XML');
          send(JSON.stringify({ event: 'snapshot', requestId: data.requestId, xml }));
        } catch { exportError(data.requestId); }
        return;
      }
      if (data?.action === 'cancel-export') {
        exporting = null;
        ui?.editor.graph.setEnabled(true);
        ui?.spinner.stop();
        return;
      }
      if (data?.action === 'export' && validId(data.requestId)) {
        try {
          if (data.format !== 'xmlpng' || !validXml(data.xml) || !ui.editor.isExportToCanvas() ||
            ui.getFileData(true) !== data.xml) throw new Error('invalid export');
          exporting = { requestId: data.requestId, failed: false };
          // Avoid setFileData during export: reloading XML would discard the undo history.
          data = { action: 'export', format: 'xmlpng', requestId: data.requestId };
        } catch { exportError(data.requestId); return; }
      } else if (data?.action !== 'load') return;
      window.dispatchEvent(new MessageEvent('message', {
        data: JSON.stringify(data), source: port
      }));
    };
    port.start();
    window.removeEventListener('message', bind);
    window.checkAllLoaded();
  });
  window.parent.postMessage({ event: 'notryx-drawio-ready' }, '*');
})();
