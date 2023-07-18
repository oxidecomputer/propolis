// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::marker::PhantomData;
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use crate::vmm::{MemCtx, VmmHdl};

type AccId = usize;

const ID_ROOT: AccId = 1;

type TreeBackref<T> = Arc<Mutex<Tree<T>>>;

struct Tree<T> {
    resource_root: Option<Arc<T>>,
    nodes: BTreeMap<AccId, Weak<NodeInner<T>>>,
    children: BTreeMap<AccId, BTreeSet<AccId>>,
    next_id: AccId,
}
impl<T> Tree<T> {
    fn adopt(
        &mut self,
        leaf: AccId,
        child: &mut Tree<T>,
        parent_ref: &TreeBackref<T>,
        mut child_root: MutexGuard<NodeEntry<T>>,
    ) {
        assert!(
            self.nodes.get(&leaf).and_then(Weak::upgrade).is_some(),
            "leaf target for reparenting missing"
        );

        let mut queue = VecDeque::new();
        queue.push_back((ID_ROOT, leaf));
        while let Some((child_id, local_parent_id)) = queue.pop_front() {
            if let Some(node) =
                child.nodes.remove(&child_id).and_then(|n| Weak::upgrade(&n))
            {
                let new_child_id = self.next_id();

                if child_id != ID_ROOT {
                    let mut ent = node.ent.lock().unwrap();
                    // Place the node in our tree
                    debug_assert_eq!(ent.id, child_id);
                    ent.id = new_child_id;
                    ent.tree = Arc::clone(parent_ref);
                    drop(ent);
                } else {
                    // Processing for the child root node is special, as we
                    // already hold the lock on it.
                    debug_assert_eq!(child_root.id, child_id);
                    child_root.id = new_child_id;
                    child_root.tree = Arc::clone(parent_ref);
                }

                self.nodes.insert(new_child_id, Arc::downgrade(&node));
                self.add_child(local_parent_id, new_child_id);

                // Associate any resource we have
                if let Some(res) = self.resource_root.as_ref() {
                    let mut local = node.local.lock().unwrap();
                    local.resource = Some(res.clone());
                }

                // Note its children so they can be processed in turn
                if let Some(cq) = child.children.remove(&child_id) {
                    queue.extend(cq.into_iter().map(|cid| (cid, new_child_id)));
                }
            }
        }
        debug_assert!(child.nodes.is_empty());
    }
    fn add_child(&mut self, parent: AccId, child: AccId) {
        debug_assert!(
            self.nodes.get(&parent).is_some(),
            "parent node must exist"
        );
        debug_assert!(
            self.nodes.get(&child).is_some(),
            "child node must exist"
        );
        if let Some(children) = self.children.get_mut(&parent) {
            children.insert(child);
        } else {
            let mut children = BTreeSet::new();
            children.insert(child);
            self.children.insert(parent, children);
        }
    }
    fn next_id(&mut self) -> AccId {
        let res = self.next_id;
        self.next_id += 1;
        res
    }

    fn new(resource: Option<Arc<T>>) -> Node<T> {
        let tree = Arc::new(Mutex::new(Tree {
            resource_root: resource.clone(),
            nodes: BTreeMap::new(),
            children: BTreeMap::new(),
            next_id: ID_ROOT + 1,
        }));
        let node = Arc::new(NodeInner {
            ent: Mutex::new(NodeEntry { id: ID_ROOT, tree: tree.clone() }),
            local: Mutex::new(NodeLocal { resource }),
        });
        let mut guard = tree.lock().unwrap();
        guard.nodes.insert(ID_ROOT, Arc::downgrade(&node));

        node
    }
}

struct NodeEntry<T> {
    id: AccId,
    tree: TreeBackref<T>,
}
struct NodeLocal<T> {
    resource: Option<Arc<T>>,
    // TODO: store local enable/disable state here for propagation
}
struct NodeInner<T> {
    ent: Mutex<NodeEntry<T>>,
    local: Mutex<NodeLocal<T>>,
}
impl<T> NodeInner<T> {
    fn tree(&self) -> Arc<Mutex<Tree<T>>> {
        let guard = self.ent.lock().unwrap();
        guard.tree.clone()
    }

    fn set_parent(&self, parent: &NodeInner<T>) {
        assert_ne!(
            self as *const NodeInner<_>, parent as *const NodeInner<_>,
            "cannot adopt to self"
        );

        let child_tree = self.tree();
        let mut child_guard = child_tree.lock().unwrap();
        let child_ent = self.ent.lock().unwrap();
        assert_eq!(child_ent.id, ID_ROOT, "adopting of non-roots not allowed");

        let parent_tree = parent.tree();
        let mut parent_guard = parent_tree.lock().unwrap();
        let parent_ent = parent.ent.lock().unwrap();
        parent_guard.adopt(
            parent_ent.id,
            &mut child_guard,
            &parent_ent.tree,
            child_ent,
        );
    }

    fn new_child(&self) -> Arc<NodeInner<T>> {
        let tree = self.tree();
        let mut guard = tree.lock().unwrap();
        let parent = self.ent.lock().unwrap();

        let child_id = guard.next_id();
        let child = Arc::new(NodeInner {
            ent: Mutex::new(NodeEntry {
                id: child_id,
                tree: parent.tree.clone(),
            }),
            local: Mutex::new(NodeLocal {
                resource: guard.resource_root.clone(),
            }),
        });

        guard.nodes.insert(child_id, Arc::downgrade(&child));
        guard.add_child(parent.id, child_id);

        child
    }

    fn guard(&self) -> Option<Guard<'_, T>> {
        let local = self.local.lock().unwrap();
        local
            .resource
            .as_ref()
            .map(|res| Guard { inner: res.clone(), _pd: PhantomData })
    }

    fn poison(&self) -> Option<Arc<T>> {
        let tree = self.tree();
        let mut tree_guard = tree.lock().unwrap();
        let ent = self.ent.lock().unwrap();
        assert_eq!(ent.id, ID_ROOT, "tree poisoning only allowed at root");
        tree_guard.resource_root.take()
    }
}
type Node<T> = Arc<NodeInner<T>>;

pub struct Guard<'a, T> {
    inner: Arc<T>,
    _pd: PhantomData<&'a T>,
}
impl<T> std::borrow::Borrow<T> for Guard<'_, T> {
    fn borrow(&self) -> &T {
        &self.inner
    }
}
impl<T> std::ops::Deref for Guard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.inner
    }
}
pub fn opt_guard<'a, T>(val: &'a Option<Guard<'_, T>>) -> Option<&'a T> {
    val.as_ref().map(std::ops::Deref::deref)
}

pub struct Accessor<T>(Node<T>);
impl<T> Accessor<T> {
    pub(crate) fn new(res: Arc<T>) -> Self {
        Self(Tree::new(Some(res)))
    }
    pub fn new_orphan() -> Self {
        Self(Tree::new(None))
    }

    pub fn child(&self) -> Self {
        Self(self.0.new_child())
    }
    pub fn set_parent(&self, parent: &Self) {
        self.0.set_parent(&parent.0);
    }
    pub fn poison(&self) -> Option<Arc<T>> {
        self.0.poison()
    }

    pub fn access(&self) -> Option<Guard<T>> {
        self.0.guard()
    }
}

pub type MemAccessor = Accessor<MemCtx>;

// Keep the rest of VmmHdl hidden for the MSI accessor
pub struct MsiAccessor(Accessor<VmmHdl>);
impl MsiAccessor {
    pub fn new(res: Arc<VmmHdl>) -> Self {
        Self(Accessor::new(res))
    }
    pub fn new_orphan() -> Self {
        Self(Accessor::new_orphan())
    }
    pub fn child(&self) -> Self {
        Self(self.0.child())
    }
    pub fn set_parent(&self, parent: &Self) {
        self.0.set_parent(&parent.0)
    }
    pub fn poison(&self) -> Option<Arc<VmmHdl>> {
        self.0.poison()
    }
    pub fn send(&self, addr: u64, msg: u64) -> Result<(), ()> {
        if let Some(guard) = self.0.access() {
            guard.lapic_msi(addr, msg).expect("lapic_msi() should succeed");
            Ok(())
        } else {
            Err(())
        }
    }
}
impl std::fmt::Debug for MsiAccessor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MsiAccessor").finish()
    }
}
