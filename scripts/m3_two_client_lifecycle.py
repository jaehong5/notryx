"""Normal two-client acceptance flow, using the existing isolated WebView fixture."""
import hashlib
import json
import re
import sqlite3
import time


def verify_lifecycle(helpers, clients, notes, state, root_id, folder_id,
                     server_power, probe_port, remote_requests):
    require = helpers.require
    deadline = time.monotonic() + 300

    def wait(check, description):
        until = min(deadline, time.monotonic() + 60)
        while time.monotonic() < until:
            value = check()
            if value:
                return value
            time.sleep(0.1)
        raise helpers.TestFailure("lifecycle: timed out waiting for " + description)

    def rows(index, query, parameters=()):
        with sqlite3.connect(f"file:{clients[index][3] / 'drafts.sqlite3'}?mode=ro",
                             uri=True, timeout=5) as db:
            return db.execute(query, parameters).fetchall()

    def focus(index):
        focus = helpers.capture_remote_change_focus(exclude_window=clients[1-index][4])
        try:
            require(focus[2] == clients[index][4], "lifecycle focused an unexpected window")
        finally:
            focus[0].XCloseDisplay(focus[1])
        wait(lambda: clients[index][0]("return document.hasFocus()"), "client focus")

    def click(index, selector):
        js = clients[index][0]
        wait(lambda: js("""const button=document.querySelector(arguments[0]);
            if (!button || button.disabled) return false;
            button.click(); return true;""", selector), selector)

    def field(index, selector, value):
        clients[index][0]("""const e=document.querySelector(arguments[0]);
            const type=e.tagName==='SELECT' ? HTMLSelectElement : HTMLInputElement;
            Object.getOwnPropertyDescriptor(type.prototype,'value').set.call(e,arguments[1]);
            e.dispatchEvent(new Event(e.tagName==='SELECT'?'change':'input',{bubbles:true}));""", selector, value)

    def panel(index, name):
        js = clients[index][0]
        wait(lambda: js("""return !!document.querySelector('[aria-label="'+arguments[0]+'"]') ||
            [...document.querySelectorAll('button')].some(b=>b.textContent.trim()===arguments[0]&&!b.disabled)""", name), name)
        js("""if(!document.querySelector('[aria-label="'+arguments[0]+'"]'))
            [...document.querySelectorAll('button')].find(b=>b.textContent.trim()===arguments[0]).click()""", name)

    def operations(index):
        return rows(index, """SELECT o.operation_id,o.operation_seq,o.operation_kind,o.document_id,
            o.expected_revision,o.bytes,o.status,o.terminal_kind,s.confirmed_through,r.device_stream_id
            FROM client_outbox o JOIN client_registrations r ON r.id=o.registration_id
            JOIN outbox_streams s ON s.registration_id=r.id WHERE r.status='active'
            ORDER BY o.operation_seq""")

    def confirmed(index, operation_id):
        found = [r for r in operations(index) if r[0] == operation_id]
        if not found or found[0][6:8] != ('committed', None) or found[0][8] < found[0][1]:
            return None
        row = found[0]
        with sqlite3.connect(f"file:{state / 'state.db'}?mode=ro", uri=True) as db:
            retired = db.execute("SELECT retired_through FROM operation_streams WHERE stream_id=?", (row[9],)).fetchone()
        return row if retired and int(retired[0]) >= row[1] else None

    def new_operation(index, before):
        found = [r for r in operations(index) if r[0] not in before]
        require(len(found) <= 1, "lifecycle action created duplicate operations")
        return found[0] if found else None

    def perform(index, action):
        before = {r[0] for r in operations(index)}
        action()
        operation = wait(lambda: new_operation(index, before), "new durable operation")
        return wait(lambda: confirmed(index, operation[0]), "server receipt and confirmed ACK")

    def node(index, node_id):
        found = rows(index, """SELECT n.state FROM remote_nodes n JOIN client_registrations r
            ON r.id=n.registration_id WHERE r.status='active' AND n.node_id=?""", (node_id,))
        return json.loads(found[0][0]) if found else None

    def converge(node_id, name, parent, deleted=False):
        evidence = []
        for index in (0, 1):
            focus(index)
            def current():
                value = node(index, node_id)
                return value if value and value['name'] == name and value['parent_id'] == parent and value['deleted'] == deleted else None
            evidence.append(wait(current, "matching metadata on both clients"))
            js = clients[index][0]
            wait(lambda: js("""return !!document.querySelector('[data-document="'+arguments[0]+'"]') === arguments[1]""", node_id, not deleted), "matching document list")
        require(evidence[0] == evidence[1], "two client metadata states differ")

    def open_document(index, node_id, text):
        focus(index)
        click(index, f'[data-document="{node_id}"]')
        try:
            wait(lambda: clients[index][0]("""return document.querySelector('[aria-current=page]')?.dataset.document===arguments[0]
                && document.querySelector('.cm-content[contenteditable=true]')?.cmTile.root.view.state.doc.toString()===arguments[1]""", node_id, text), "exact editable source")
        except helpers.TestFailure:
            diagnostic = clients[index][0]("""const editor=document.querySelector('.cm-content');
                return {selected:document.querySelector('[aria-current=page]')?.dataset.document===arguments[0],
                    editable:editor?.getAttribute('contenteditable'),focused:document.hasFocus(),
                    actual_length:editor?.cmTile.root.view.state.doc.length,expected_length:arguments[1].length,
                    matches:editor?.cmTile.root.view.state.doc.toString()===arguments[1],
                    alerts:[...document.querySelectorAll('[role=alert]')].map(e=>(e.textContent||'').trim())
                        .filter(value=>/^[A-Za-z_]{1,80}$/.test(value))};""", node_id, text)
            print(f'M3 lifecycle open diagnostic: client={index} {json.dumps(diagnostic)}', flush=True)
            raise

    def edit(index, text):
        clients[index][0]("""const v=document.querySelector('.cm-content').cmTile.root.view;
            v.dispatch({changes:{from:0,to:v.state.doc.length,insert:arguments[0]},userEvent:'input.type'});
            document.querySelector('#save').click();""", text)

    def close_document(index):
        click(index, '[data-testid=close-document]')
        wait(lambda: clients[index][0]("return !document.querySelector('.cm-content')"), "closed editor")

    focus(0)
    panel(0, '새 폴더 또는 문서')
    wait(lambda: clients[0][0]("""return !!document.querySelector('[data-testid=create-parent] option[value="'+arguments[0]+'"]')""", root_id), "create parent")
    field(0, '[data-testid=create-parent]', root_id)
    field(0, '[data-testid=create-kind]', 'file')
    field(0, '[data-testid=create-name]', 'Lifecycle.md')
    created = perform(0, lambda: click(0, '[data-testid=create-submit]'))
    node_id = created[3]
    require((notes / 'Lifecycle.md').read_bytes() == b'', "new document was not empty")
    converge(node_id, 'Lifecycle.md', root_id)
    source = '\n\n'.join([
        '# 통합 검증', '<script>window.previewExecuted=true</script>',
        "<svg onload='window.previewExecuted=true'></svg>",
        '[위험 링크](javascript:alert(1))', f'![외부 이미지](http://127.0.0.1:{probe_port}/image)',
    ]) + '\n'
    open_document(0, node_id, '')
    perform(0, lambda: edit(0, source))
    open_document(1, node_id, source)
    require((notes / 'Lifecycle.md').read_bytes() == source.encode(), "source bytes changed")
    print('M3 lifecycle: UI creation, edit and peer source converged.', flush=True)

    for index in (0, 1):
        focus(index)
        js, wd, session, _, _ = clients[index]
        click(index, '#preview-toggle')
        wait(lambda: js("return document.querySelector('.markdown-preview h1')?.textContent==='통합 검증'"), "security preview")
        require(js("return !window.previewExecuted && !document.querySelector('.markdown-preview script,.markdown-preview svg,.markdown-preview img,.markdown-preview a')"), "unsafe preview content rendered")
        denials = js("""const done=arguments[arguments.length-1];
            Promise.all([['plugin:fs|read_text_file',{path:'/notryx-probe-must-not-exist'}],
                ['plugin:shell|execute',{program:'notryx-probe-must-not-exist'}],
                ['plugin:sql|load',{db:'sqlite:probe.db'}]].map(async ([command,args])=>{
                    try {await window.__TAURI_INTERNALS__.invoke(command,args);return null;}
                    catch(error){return String(error);}})).then(done);""", asynchronous=True)
        require(len(denials) == 3 and all(isinstance(e, str) and re.search(r'not allowed|^plugin (fs|shell|sql) not found$', e, re.I) for e in denials), "main WebView gained filesystem, shell or SQL access")
        click(index, '#preview-toggle')
        owner = js("const done=arguments[arguments.length-1];window.__TAURI_INTERNALS__.invoke('m0_renderer_session').then(done)", asynchronous=True)
        initial = wd('GET', f'/session/{session}/window')
        click(index, '#isolation-open')
        handles = wait(lambda: (h if len(h := wd('GET', f'/session/{session}/window/handles')) == 2 else None), "untrusted WebView")
        wd('POST', f'/session/{session}/window', {'handle': next(h for h in handles if h != initial)})
        wait(lambda: js("return document.querySelector('#result')?.textContent.startsWith('{')"), "isolation probe")
        denial = js("""const done=arguments[arguments.length-1];
            window.__TAURI_INTERNALS__.invoke('workspace_inventory',{request:{renderer_session_id:arguments[0],after_id:null}})
            .then(()=>done(null)).catch(e=>done(String(e)));""", owner, asynchronous=True)
        require(isinstance(denial, str) and any(s in denial.lower() for s in ('not allowed','denied','invalid_caller')), "untrusted WebView accessed workspace IPC")
        wd('DELETE', f'/session/{session}/window')
        wd('POST', f'/session/{session}/window', {'handle': initial})
    require(not remote_requests, "preview requested an external image")
    print('M3 lifecycle: both apps blocked unsafe preview content and privileged IPC.', flush=True)

    focus(0)
    server_power(False)
    offline = source + '\n서버 중지 중 저장\n'
    before = {r[0] for r in operations(0)}
    edit(0, offline)
    pending = wait(lambda: new_operation(0, before), "offline durable outbox")
    require(pending[5] == offline.encode() and pending[6] in ('queued', 'sent'), "offline outbox bytes or state differ")
    require((notes / 'Lifecycle.md').read_bytes() == source.encode(), "offline edit changed server source")
    server_power(True)
    done = wait(lambda: confirmed(0, pending[0]), "same offline operation after server restart")
    require(done[:6] == pending[:6], "reconnect changed operation identity or payload")
    require((notes / 'Lifecycle.md').read_bytes() == offline.encode(), "reconnected server source differs")
    focus(1)
    wait(lambda: clients[1][0]("return document.querySelector('.cm-content')?.cmTile.root.view.state.doc.toString()===arguments[0]", offline), "peer source after reconnect")
    returned = offline + '\n다른 기기 후속 편집\n'
    perform(1, lambda: edit(1, returned))
    focus(0)
    wait(lambda: clients[0][0]("return document.querySelector('.cm-content')?.cmTile.root.view.state.doc.toString()===arguments[0]", returned), "reverse edit after reconnect")
    require((notes / 'Lifecycle.md').read_bytes() == returned.encode(), "reverse edit source differs")
    print('M3 lifecycle: server restart preserved the queued operation; both edit directions converged.', flush=True)

    for index in (0, 1):
        focus(index)
        close_document(index)
    focus(0)
    click(0, f'[data-move-node="{node_id}"]')
    wait(lambda: clients[0][0]("""return !!document.querySelector('[data-testid=move-parent] option[value="'+arguments[0]+'"]')""", folder_id), "move destination")
    field(0, '[data-testid=move-parent]', folder_id)
    field(0, '[data-testid=move-name]', 'Moved.md')
    click(0, '[data-testid=move-prepare]')
    perform(0, lambda: click(0, '[data-testid=move-confirm]'))
    converge(node_id, 'Moved.md', folder_id)
    require(not (notes / 'Lifecycle.md').exists() and (notes / 'Test Folder' / 'Moved.md').read_bytes() == returned.encode(), "move changed source bytes")

    focus(1)
    click(1, f'[data-delete-node="{node_id}"]')
    perform(1, lambda: click(1, '[data-testid=delete-confirm]'))
    converge(node_id, 'Moved.md', folder_id, deleted=True)
    require(not (notes / 'Test Folder' / 'Moved.md').exists(), "deleted source remained live")
    preserved = state / 'objects' / 'sha256' / hashlib.sha256(returned.encode()).hexdigest()
    require(preserved.read_bytes() == returned.encode(), "deleted source object was not preserved")

    focus(0)
    panel(0, '휴지통')
    click(0, f'[data-trash-node="{node_id}"]')
    wait(lambda: clients[0][0]("""return !!document.querySelector('[data-testid=restore-parent] option[value="'+arguments[0]+'"]')""", folder_id), "restore destination")
    field(0, '[data-testid=restore-parent]', folder_id)
    perform(0, lambda: click(0, '[data-testid=restore-submit]'))
    converge(node_id, 'Moved.md', folder_id)
    for index in (0, 1):
        open_document(index, node_id, returned)
    require((notes / 'Test Folder' / 'Moved.md').read_bytes() == returned.encode(), "restore changed source bytes")
    require(preserved.read_bytes() == returned.encode(), "restore changed preserved object")
    require(not remote_requests, "lifecycle caused an external image request")
    print('M3 two-client lifecycle passed: UI CRUD, exact bytes, confirmed ACKs, reconnect, two-way editing and renderer boundaries.', flush=True)
