// Temporary probe: print every notify event delivered for /tmp/nwtest.
use notify::Watcher;

fn main() {
    let (tx, rx) = std::sync::mpsc::channel();
    let mut w = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let _ = tx.send(res);
    })
    .unwrap();
    w.watch(std::path::Path::new("/tmp/nwtest"), notify::RecursiveMode::NonRecursive)
        .unwrap();
    println!("watching /tmp/nwtest");
    for res in rx.iter() {
        if let Ok(ev) = res {
            println!("{:?} {:?}", ev.kind, ev.paths);
        }
    }
}
