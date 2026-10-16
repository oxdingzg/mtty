use super::*;

#[test]
fn two_remote_hosts_keep_diagnostics_and_definition_targets_separate() {
    let directory = std::env::temp_dir().join(format!(
        "mtty-lsp-scope-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&directory).unwrap();
    let script = directory.join("server.py");
    std::fs::write(&script,r#"import json,sys
def send(v):
 data=json.dumps(v).encode();sys.stdout.buffer.write(('Content-Length: %d\r\n\r\n'%len(data)).encode()+data);sys.stdout.buffer.flush()
while True:
 headers={}
 while True:
  line=sys.stdin.buffer.readline()
  if not line:sys.exit(0)
  if line in (b'\r\n',b'\n'):break
  k,v=line.decode().split(':',1);headers[k.lower()]=v.strip()
 req=json.loads(sys.stdin.buffer.read(int(headers['content-length'])));method=req.get('method');rid=req.get('id');p=req.get('params',{})
 if method=='initialize':
  assert p['processId'] is None
  assert p['rootUri'].startswith('file:///remote-')
  send({'jsonrpc':'2.0','id':rid,'result':{'capabilities':{'hoverProvider':True,'completionProvider':{},'definitionProvider':True}}})
 elif method=='textDocument/didOpen':
  doc=p['textDocument'];assert doc['uri']=='file:///same.rs'
  send({'jsonrpc':'2.0','method':'textDocument/publishDiagnostics','params':{'uri':doc['uri'],'diagnostics':[{'range':{'start':{'line':0,'character':0},'end':{'line':0,'character':1}},'message':doc['text']}]}})
 elif method=='textDocument/definition':send({'jsonrpc':'2.0','id':rid,'result':{'uri':'file:///other.rs','range':{'start':{'line':0,'character':0},'end':{'line':0,'character':1}}}})
 elif method=='shutdown':send({'jsonrpc':'2.0','id':rid,'result':None})
 elif method=='exit':break
"#).unwrap();
    let mut settings = Settings::default();
    for target in ["a.example.invalid", "b.example.invalid"] {
        settings.remotes.insert(
            target.into(),
            remote::Workspace {
                root: format!("/remote-{target}").into(),
                prefix: vec!["/usr/bin/python3".into(), script.display().to_string()],
                servers: HashMap::new(),
            },
        );
    }
    let mut lsp = Lsp::new(settings, std::sync::Arc::new(|| {}));
    let a = remote::document_key("a.example.invalid", "/same.rs");
    let b = remote::document_key("b.example.invalid", "/same.rs");
    lsp.sync_remote(
        &a,
        Path::new("/same.rs"),
        Path::new("/remote-a"),
        Some("Rust"),
        &Rope::from_str("host A"),
        0,
    );
    lsp.sync_remote(
        &b,
        Path::new("/same.rs"),
        Path::new("/remote-b"),
        Some("Rust"),
        &Rope::from_str("host B"),
        0,
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while (lsp.diagnostics(&a).is_empty() || lsp.diagnostics(&b).is_empty())
        && std::time::Instant::now() < deadline
    {
        lsp.poll();
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(
        lsp.diagnostics(&a)
            .first()
            .map(|diagnostic| diagnostic.message.as_str()),
        Some("host A")
    );
    assert_eq!(
        lsp.diagnostics(&b)
            .first()
            .map(|diagnostic| diagnostic.message.as_str()),
        Some("host B")
    );
    assert!(lsp.definition(&a, &Rope::from_str("host A"), 0));
    let mut target = None;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while target.is_none() && std::time::Instant::now() < deadline {
        for event in lsp.poll() {
            if let Event::Definition {
                source, targets, ..
            } = event
            {
                assert_eq!(source, a);
                target = targets.into_iter().next().map(|(path, _)| path);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(
        remote::split_key(&target.unwrap()),
        Some(("a.example.invalid".into(), "/other.rs".into()))
    );
    lsp.retain(&HashSet::from([b.clone()]));
    assert!(!lsp.handles(&a));
    assert!(lsp.handles(&b));
    lsp.shutdown();
    drop(lsp);
    std::fs::remove_dir_all(directory).unwrap();
}
