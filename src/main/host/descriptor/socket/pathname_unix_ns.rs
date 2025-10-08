use std::collections::HashMap;
use std::sync::{Arc, Weak};

use atomic_refcell::AtomicRefCell;

use crate::host::descriptor::listener::{StateEventSource, StateListenHandle, StateListenerFilter};
use crate::host::descriptor::socket::unix::{UnixSocket, UnixSocketType};
use crate::host::descriptor::{FileSignals, FileState};

struct NamespaceEntry {
    socket: Weak<AtomicRefCell<UnixSocket>>,
    _handle: StateListenHandle,
}

impl NamespaceEntry {
    fn new(socket: Weak<AtomicRefCell<UnixSocket>>, handle: StateListenHandle) -> Self {
        Self { socket, _handle: handle }
    }
}

pub struct PathnameUnixNamespace {
    by_type: HashMap<UnixSocketType, HashMap<Vec<u8>, NamespaceEntry>>,
}

impl PathnameUnixNamespace {
    pub fn new() -> Self {
        let mut rv = Self { by_type: HashMap::new() };
        rv.by_type.insert(UnixSocketType::Stream, HashMap::new());
        rv.by_type.insert(UnixSocketType::Dgram, HashMap::new());
        rv.by_type.insert(UnixSocketType::SeqPacket, HashMap::new());
        rv
    }

    pub fn bind(
        ns: &Arc<AtomicRefCell<Self>>,
        sock_type: UnixSocketType,
        path: Vec<u8>,
        socket: &Arc<AtomicRefCell<UnixSocket>>,
        socket_events: &mut StateEventSource,
    ) -> Result<(), ()> {
        let mut ns_borrow = ns.borrow_mut();

        if ns_borrow.by_type.get(&sock_type).unwrap().contains_key(&path) {
            return Err(());
        }

        let path_copy = path.clone();
        let handle = Self::on_socket_close(Arc::downgrade(ns), socket_events, move |ns_mut| {
            let _ = ns_mut.unbind(sock_type, &path_copy);
        });

        ns_borrow
            .by_type
            .get_mut(&sock_type)
            .unwrap()
            .insert(path, NamespaceEntry::new(Arc::downgrade(socket), handle));

        Ok(())
    }

    pub fn lookup(
        &self,
        sock_type: UnixSocketType,
        path: &[u8],
    ) -> Option<Arc<AtomicRefCell<UnixSocket>>> {
        self.by_type
            .get(&sock_type)
            .unwrap()
            .get(path)
            .map(|e| e.socket.upgrade().unwrap())
    }

    pub fn unbind(&mut self, sock_type: UnixSocketType, path: &[u8]) -> Result<(), ()> {
        let m = self.by_type.get_mut(&sock_type).unwrap();
        if m.remove(path).is_none() {
            return Err(());
        }
        Ok(())
    }

    fn on_socket_close(
        ns: Weak<AtomicRefCell<Self>>,
        event_source: &mut StateEventSource,
        f: impl Fn(&mut Self) + Send + Sync + 'static,
    ) -> StateListenHandle {
        event_source.add_listener(
            FileState::CLOSED,
            FileSignals::empty(),
            StateListenerFilter::OffToOn,
            move |state, _changed, _signals, _cb_queue| {
                assert!(state.contains(FileState::CLOSED));
                if let Some(ns) = ns.upgrade() {
                    f(&mut ns.borrow_mut());
                }
            },
        )
    }
}

impl Default for PathnameUnixNamespace {
    fn default() -> Self { Self::new() }
}


