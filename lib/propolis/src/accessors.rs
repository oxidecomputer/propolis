// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Hierarchical access control for emulated resources
//!
//! Device emulation logic requires access to resources which may be
//! subsequently moderated by intervening parts of the emulation.
//!
//! For example: A PCI device performs DMA to guest memory.  If bus-mastering is
//! disabled on the device, or any parent bridge in its bus hierarchy, then its
//! contained emulation should fail any DMA accesses.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::marker::PhantomData;
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use crate::vmm::{MemCtx, VmmHdl};

type AccId = usize;

const ID_NULL: AccId = 0;
const ID_ROOT: AccId = 1;

struct TreeNode<T> {
    parent_id: AccId,
    node_ref: Weak<Node<T>>,
    children: BTreeSet<AccId>,
    name: Option<String>,
}
impl<T> TreeNode<T> {
    fn new(
        parent_id: AccId,
        node_ref: Weak<Node<T>>,
        name: Option<String>,
    ) -> Self {
        Self { parent_id, node_ref, children: BTreeSet::new(), name }
    }
}
struct Tree<T> {
    resource_root: Option<Arc<T>>,
    nodes: BTreeMap<AccId, TreeNode<T>>,
    next_id: AccId,
}
impl<T> Tree<T> {
    /// Get the next [AccId] for a node to be added into this tree.
    fn next_id(&mut self) -> AccId {
        let res = self.next_id;
        self.next_id += 1;
        res
    }

    /// Record a node in the tree, given its ID and the ID of its parent.
    fn add_child(
        &mut self,
        parent: AccId,
        child: AccId,
        child_node: &Arc<Node<T>>,
        name: Option<String>,
    ) {
        let conflict = self.nodes.insert(
            child,
            TreeNode::new(parent, Arc::downgrade(&child_node), name),
        );
        assert!(
            conflict.is_none(),
            "new child should not conflict with existing node"
        );

        self.nodes
            .get_mut(&parent)
            .expect("parent node must exist")
            .children
            .insert(child);
    }

    fn adopt(
        &mut self,
        leaf_ent: &NodeEntry<T>,
        adopt_tree: &mut Tree<T>,
        mut adopt_root: MutexGuard<NodeEntry<T>>,
    ) {
        debug_assert!(
            self.nodes
                .get(&leaf_ent.id)
                .and_then(|n| Weak::upgrade(&n.node_ref))
                .is_some(),
            "leaf target for re-parenting missing"
        );
        assert_eq!(adopt_root.id, ID_ROOT);
        let tree_ref = &leaf_ent.tree;

        let mut queue = VecDeque::new();
        queue.push_back((ID_ROOT, leaf_ent.id));
        while let Some((child_id, local_parent_id)) = queue.pop_front() {
            if let Some(mut tnode) = adopt_tree.nodes.remove(&child_id) {
                let node = match Weak::upgrade(&tnode.node_ref) {
                    Some(nr) => nr,
                    None => {
                        continue;
                    }
                };

                let new_child_id = self.next_id();

                if child_id != ID_ROOT {
                    let mut ent = node.0.lock().unwrap();
                    // Place the node in our tree
                    debug_assert_eq!(ent.id, child_id);
                    ent.id = new_child_id;
                    ent.tree = Arc::clone(tree_ref);
                    ent.resource = self.resource_root.clone();
                    drop(ent);
                } else {
                    // Processing for the child root node is special, as we
                    // already hold the lock on it.
                    debug_assert_eq!(adopt_root.id, child_id);
                    adopt_root.id = new_child_id;
                    adopt_root.tree = Arc::clone(tree_ref);
                    adopt_root.resource = self.resource_root.clone();
                }

                self.add_child(
                    local_parent_id,
                    new_child_id,
                    &node,
                    tnode.name.take(),
                );

                // Note its children so they can be processed in turn
                let cq = std::mem::take(&mut tnode.children);
                if !cq.is_empty() {
                    queue.extend(cq.into_iter().map(|cid| (cid, new_child_id)));
                }
            }
        }
        debug_assert!(adopt_tree.nodes.is_empty());
    }

    /// Remove traces of a node from the tree as it is dropped
    fn remove_dead_node(&mut self, id: AccId) {
        let mut tnode =
            self.nodes.remove(&id).expect("tree node should be present");

        if tnode.parent_id != ID_NULL {
            let removed = self
                .nodes
                .get_mut(&tnode.parent_id)
                .expect("parent for node exists")
                .children
                .remove(&id);
            assert!(removed, "parent should list node as child");
        } else {
            assert_eq!(id, ID_ROOT);
        }

        // The node is (dropping) dead, so it should no longer be reachable via
        // the Arc<> reference.
        debug_assert_eq!(tnode.node_ref.strong_count(), 0);

        // orphan any children of the node
        for child in std::mem::take(&mut tnode.children) {
            self.orphan_node(child);
        }
    }

    /// Remove a node from this Tree into a new empty tree, with all of its
    /// descendants in tow.
    fn orphan_node(&mut self, id: AccId) {
        let mut tnode =
            self.nodes.remove(&id).expect("node-to-orphan is present in tree");
        let node =
            tnode.node_ref.upgrade().expect("node-to-orphan is still live");

        let tree = Self::new_empty(None);
        let mut guard = node.0.lock().unwrap();
        // This node now becomes the root of the orphaned tree
        guard.id = ID_ROOT;
        guard.tree = tree.clone();
        guard.resource.take();
        drop(guard);
        let mut tguard = tree.lock().unwrap();
        tguard.nodes.insert(
            ID_ROOT,
            TreeNode::new(ID_NULL, Arc::downgrade(&node), None),
        );

        let children = std::mem::take(&mut tnode.children);
        if children.is_empty() {
            return;
        }
        drop(node);
        drop(tnode);

        let mut needs_fixup = VecDeque::new();
        needs_fixup.extend(children);

        while let Some(child) = needs_fixup.pop_front() {
            let mut tnode =
                self.nodes.remove(&child).expect("child tree node is present");

            // Progeny of the orphaned node which are still "live" need to be
            // associated with the new tree.  Anything which happens to be
            // "dead" will clean itself from the existing tree and orphan its
            // subsequent progeny when given access to the tree lock.
            if let Some(node) = tnode.node_ref.upgrade() {
                let new_parent_id = if tnode.parent_id == id {
                    // Direct children of the now-root orphan node need their
                    // parent_id updated.  Others can be left alone in that
                    // sense, since only the root requires an updated ID.
                    ID_ROOT
                } else {
                    tnode.parent_id
                };

                tguard.add_child(
                    new_parent_id,
                    child,
                    &node,
                    tnode.name.take(),
                );
                let mut ent = node.0.lock().unwrap();
                ent.tree = tree.clone();
                ent.resource = None;
                drop(ent);

                needs_fixup.extend(std::mem::take(&mut tnode.children))
            }
        }
    }

    fn rename_node(&mut self, id: AccId, name: Option<String>) {
        if let Some(tnode) = self.nodes.get_mut(&id) {
            tnode.name = name;
        }
    }

    fn for_each(&self, start: AccId, mut f: impl FnMut(AccId, &TreeNode<T>)) {
        let mut to_process = VecDeque::new();
        to_process.push_back(start);
        while let Some(id) = to_process.pop_front() {
            if let Some(tnode) = self.nodes.get(&id) {
                f(id, tnode);
                to_process.extend(tnode.children.iter());
            }
        }
    }

    /// Traverse tree in order conducive to printing, applying a provided
    /// `print_fn` to each node.
    fn print(&self, print_fn: impl Fn(PrintNode)) {
        // Seed the root of the tree to be processed at depth 0
        let mut initial = BTreeSet::new();
        initial.insert(ID_ROOT);
        let mut to_process = vec![(0, initial)];

        while let Some((depth, mut children)) = to_process.pop() {
            let id = match children.pop_first() {
                Some(i) => {
                    to_process.push((depth, children));
                    i
                }
                None => continue,
            };

            if let Some(tnode) = self.nodes.get(&id) {
                let pnode =
                    PrintNode { depth, id, name: tnode.name.as_deref() };
                print_fn(pnode);
                if !tnode.children.is_empty() {
                    to_process.push((depth + 1, tnode.children.clone()))
                }
            }
        }
    }

    fn new_empty(resource: Option<Arc<T>>) -> Arc<Mutex<Tree<T>>> {
        Arc::new(Mutex::new(Tree {
            resource_root: resource,
            nodes: BTreeMap::new(),
            next_id: ID_ROOT + 1,
        }))
    }
    fn new(resource: Option<Arc<T>>) -> Arc<Node<T>> {
        let tree = Self::new_empty(resource.clone());
        let node = Node::new_root(tree.clone());
        node.0.lock().unwrap().resource = resource;

        let mut tguard = tree.lock().unwrap();
        tguard.nodes.insert(
            ID_ROOT,
            TreeNode::new(ID_NULL, Arc::downgrade(&node), None),
        );

        node
    }
}

/// Data provided to `print_fn` callback as part of [`Tree::print()`]
pub struct PrintNode<'a> {
    pub depth: usize,
    pub id: AccId,
    pub name: Option<&'a str>,
}

/// Build printing function for [`Tree::print()`] which outputs a list format.
pub fn print_basic(match_node: Option<AccId>) -> impl Fn(PrintNode) {
    move |node| {
        let id = node.id;
        let pad = "  ".repeat(node.depth);
        let highlight = if Some(id) == match_node { " ***" } else { "" };
        let namestr = match node.name {
            None if id == ID_ROOT => "'ROOT'".to_string(),
            None => "<unnamed>".to_string(),
            Some(s) => format!("'{s}'"),
        };

        println!("{pad}- {{ id: {id}, name: {namestr} }}{highlight}");
    }
}

type TreeBackref<T> = Arc<Mutex<Tree<T>>>;

struct NodeEntry<T> {
    id: AccId,
    tree: TreeBackref<T>,
    resource: Option<Arc<T>>,
    // TODO: store enable/disable state here for evaluation and propagation
}
struct Node<T>(Mutex<NodeEntry<T>>);
impl<T> Node<T> {
    /// Lock tree and entry (in that order, as required), and check if the tree
    /// we locked is the one this node is associated with.
    ///
    /// If the tree references match, the two guards are returned. If not, the
    /// tree to which we are now associated is returned instead.
    ///
    /// This is purely a helper function to make lifetimes clearer for
    /// [`Self::lock_tree()`]
    #[allow(clippy::type_complexity)]
    fn try_lock_tree<'a>(
        &'a self,
        tree_ref: &'a TreeBackref<T>,
    ) -> Result<
        (MutexGuard<'a, Tree<T>>, MutexGuard<'a, NodeEntry<T>>),
        TreeBackref<T>,
    > {
        let tguard = tree_ref.lock().unwrap();
        let guard = self.0.lock().unwrap();
        if Arc::ptr_eq(tree_ref, &guard.tree) {
            Ok((tguard, guard))
        } else {
            Err(guard.tree.clone())
        }
    }

    /// Safely acquire the lock to this entry, as well as the containing tree,
    /// respecting the ordering requirements.
    fn lock_tree<R>(
        &self,
        f: impl FnOnce(MutexGuard<'_, Tree<T>>, MutexGuard<'_, NodeEntry<T>>) -> R,
    ) -> R {
        let mut tree = self.0.lock().unwrap().tree.clone();
        let (tguard, ent) = loop {
            let new_tree = match self.try_lock_tree(&tree) {
                Ok((tg, g)) => break (tg, g),
                Err(nt) => nt,
            };
            let _ = std::mem::replace(&mut tree, new_tree);
        };
        f(tguard, ent)
    }

    fn adopt(&self, child: &Node<T>, name: Option<String>) {
        assert_ne!(
            self as *const Node<_>, child as *const Node<_>,
            "cannot adopt self"
        );

        self.lock_tree(|mut parent_tguard, parent_ent| {
            child.lock_tree(|mut child_tguard, child_ent| {
                if child_ent.id != ID_ROOT {
                    // Drop all mutex guards prior to panic in order to allow
                    // unwinder to do its job, rather than getting tripped up by
                    // poisoned mutexes.  This allows the unit tests to exercise
                    // this panic condition.
                    drop(child_tguard);
                    drop(child_ent);
                    drop(parent_tguard);
                    drop(parent_ent);
                    panic!("adopting of non-roots not allowed");
                }
                // Apply the chosen name to the root prior to its adoption
                child_tguard.rename_node(ID_ROOT, name);
                parent_tguard.adopt(&parent_ent, &mut child_tguard, child_ent);
            });
        });
    }

    fn new_root(tree: Arc<Mutex<Tree<T>>>) -> Arc<Node<T>> {
        Arc::new(Node(Mutex::new(NodeEntry {
            id: ID_ROOT,
            tree,
            resource: None,
        })))
    }
    fn new_child(&self, name: Option<String>) -> Arc<Node<T>> {
        self.lock_tree(|mut tguard, parent| {
            let child_id = tguard.next_id();
            let child = Arc::new(Node(Mutex::new(NodeEntry {
                id: child_id,
                tree: parent.tree.clone(),
                resource: tguard.resource_root.clone(),
            })));

            tguard.add_child(parent.id, child_id, &child, name);

            child
        })
    }

    fn guard(&self) -> Option<Guard<'_, T>> {
        let local = self.0.lock().unwrap();
        local
            .resource
            .as_ref()
            .map(|res| Guard { inner: res.clone(), _pd: PhantomData })
    }

    fn poison(&self) -> Option<Arc<T>> {
        self.lock_tree(|mut tguard, ent| {
            let id = ent.id;
            drop(ent);
            if id != ID_ROOT {
                drop(tguard);
                panic!("tree poisoning only allowed at root");
            }

            // Remove the resource from the tree...
            let top_res = tguard.resource_root.take();
            // ... and poison all nodes too
            if top_res.is_some() {
                tguard.for_each(ID_ROOT, |_id, tnode| {
                    if let Some(node) = tnode.node_ref.upgrade() {
                        node.0.lock().unwrap().resource.take();
                    }
                });
            }

            top_res
        })
    }
}
impl<T> Drop for Node<T> {
    fn drop(&mut self) {
        let ent = self.0.get_mut().unwrap();
        // drop any lingering access to the resource immediately
        let _ = ent.resource.take();
        // and remove us from the tree
        ent.tree.lock().unwrap().remove_dead_node(ent.id);
    }
}

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

pub struct Accessor<T>(Arc<Node<T>>);
impl<T> Accessor<T> {
    /// Create a new accessor hierarchy, mediating access to `resource`.
    pub fn new(resource: Arc<T>) -> Self {
        Self(Tree::new(Some(resource)))
    }

    /// Create a new orphaned accessor hierarchy, bearing no existing resource.
    /// The hierarchy can gain access to a valid resource by being
    /// [adopted][`Self::adopt()`].
    pub fn new_orphan() -> Self {
        Self(Tree::new(None))
    }

    /// Create a child of this node.
    pub fn child(&self, name: Option<String>) -> Self {
        Self(self.0.new_child(name))
    }

    /// Adopt an orphan node and its descendants.
    ///
    /// # Panics
    ///
    /// If the node to be adopted is not the root of an orphan tree.
    pub fn adopt(&self, child: &Self, name: Option<String>) {
        self.0.adopt(&child.0, name);
    }

    /// Poison (remove the underlying resource) from the root node of a
    /// hierarchy.
    ///
    /// # Panics
    ///
    /// If this is called on a non-root node.
    pub fn poison(&self) -> Option<Arc<T>> {
        self.0.poison()
    }

    /// Attempt to gain access to the underlying resource.
    ///
    /// Will return [None] if any ancestor node disables access, or if the node
    /// is not attached to a hierarchy containing a valid resource.
    pub fn access(&self) -> Option<Guard<T>> {
        self.0.guard()
    }

    /// Print the hierarchy that this node is a member of
    pub fn print(&self, highlight_self: bool) {
        self.0.lock_tree(|tree, ent| {
            tree.print(print_basic(highlight_self.then_some(ent.id)));
        });
    }
}

pub type MemAccessor = Accessor<MemCtx>;

// Keep the rest of VmmHdl hidden for the MSI accessor
pub struct MsiAccessor(Accessor<VmmHdl>);
impl MsiAccessor {
    /// See: [`Accessor::new()`]
    pub fn new(resource: Arc<VmmHdl>) -> Self {
        Self(Accessor::new(resource))
    }
    /// See: [`Accessor::new_orphan()`]
    pub fn new_orphan() -> Self {
        Self(Accessor::new_orphan())
    }
    /// See: [`Accessor::child()`]
    pub fn child(&self, name: Option<String>) -> Self {
        Self(self.0.child(name))
    }
    /// See: [`Accessor::adopt()`]
    pub fn adopt(&self, child: &Self, name: Option<String>) {
        self.0.adopt(&child.0, name)
    }
    /// See: [`Accessor::poison()`]
    pub fn poison(&self) -> Option<Arc<VmmHdl>> {
        self.0.poison()
    }

    /// Attempt to send an MSI with the resource held by this accessor
    /// hierarchy.  Returns [`Ok`] if valid access to the resource exists,
    /// otherwise [`Err`].
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

#[cfg(test)]
mod test {
    //! Note regarding unwinding for `should_panic` tests:
    //!
    //! If any mutexes are held when the code under test panics, the poisoned
    //! mutex will prevent the unwinder from functioning properly when the
    //! [MutexGuard]s are dropped.  You will see several checks in the above
    //! logic which eschew [assert_eq] for a manual check which drops any held
    //! mutexes before issuing a [panic].

    use super::*;

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    // Helpers:

    fn new_root() -> Accessor<AtomicUsize> {
        Accessor::new(Arc::new(AtomicUsize::new(0)))
    }
    fn new_orphan() -> Accessor<AtomicUsize> {
        Accessor::new_orphan()
    }
    fn new_depth(
        depth: usize,
    ) -> (Accessor<AtomicUsize>, Vec<Accessor<AtomicUsize>>) {
        let root = new_root();
        let mut children: Vec<Accessor<AtomicUsize>> =
            Vec::with_capacity(depth);

        for idx in 0..depth {
            let next_child = match idx {
                0 => root.child(None),
                n => children[n - 1].child(None),
            };
            children.push(next_child);
        }
        (root, children)
    }

    #[test]
    fn tree_root() {
        let root = new_root();

        let guard = root.access();
        assert!(guard.is_some());
        let guard = guard.unwrap();
        drop(guard);

        let res = root.poison();
        assert!(res.is_some());

        assert!(root.access().is_none())
    }

    #[test]
    fn simple_orphan() {
        let root = new_root();
        let orphan = new_orphan();

        assert!(root.access().is_some());
        assert!(orphan.access().is_none());

        root.adopt(&orphan, None);
        assert!(orphan.access().is_some());
    }

    #[test]
    #[should_panic]
    fn only_root_can_poison() {
        let root = new_root();
        let child = root.child(None);

        assert!(root.access().is_some());
        assert!(child.access().is_some());

        child.poison();
    }

    #[test]
    #[should_panic]
    fn adopt_self() {
        let root = new_root();
        root.adopt(&root, None);
    }

    #[test]
    #[should_panic]
    fn adopt_nonroot() {
        let root = new_root();
        let child = new_orphan();
        let grandchild = child.child(None);

        root.adopt(&grandchild, None);
    }

    #[test]
    fn simple_depth() {
        let depth = 4;
        let (root, children) = new_depth(depth);

        // update the inner resource, checking that it's the same at all depths
        let tval = 1;
        root.access().unwrap().store(tval, Ordering::Relaxed);
        for child in children.iter() {
            assert_eq!(child.access().unwrap().load(Ordering::Relaxed), tval);
        }

        root.poison();
        for node in children.iter() {
            assert!(node.access().is_none());
        }
    }

    #[test]
    fn orphan_split() {
        let (root, children) = new_depth(5);

        // Wrap the children in Option, so we can drop one from the middle to
        // orphan its descendants
        let mut children =
            children.into_iter().map(|c| Some(c)).collect::<Vec<Option<_>>>();

        // Drop the middle node, causing its children to become orphaned
        children[2] = None;

        // Children above the "split" should be fine
        for child in children[0..2].iter().map(|c| c.as_ref().unwrap()) {
            assert!(child.access().is_some());
        }

        // Those below should be orphaned, with no access to the resource
        for child in children[3..].iter().map(|c| c.as_ref().unwrap()) {
            assert!(child.access().is_none());
        }

        let tval = 1;
        root.access().unwrap().store(tval, Ordering::Relaxed);

        // Closest available child will adopt the orphan chain
        children[1]
            .as_ref()
            .unwrap()
            .adopt(children[3].as_ref().unwrap(), None);

        // The adopted nodes should have access (and see the updated val)
        for child in children[3..].iter().map(|c| c.as_ref().unwrap()) {
            let guard = child.access().expect("resource is accessible");
            assert_eq!(guard.load(Ordering::Relaxed), tval);
        }
    }

    #[test]
    fn orphan_sibling() {
        let (root, mut children) = new_depth(2);

        let sib = root.child(Some("sibling".to_string()));
        let sib_child = sib.child(Some("sibling child".to_string()));

        // Check that both siblings, and their progeny, can access the resource
        assert!(sib.access().is_some());
        assert!(children[0].access().is_some());
        assert!(sib_child.access().is_some());
        assert!(children[1].access().is_some());

        // ... and that after orphaning one of them, that the other sibling
        // still has access
        let _ = children.remove(0);
        assert!(children[0].access().is_none());
        assert!(sib.access().is_some());
        assert!(sib_child.access().is_some());
    }

    #[test]
    fn print_names() {
        let root = new_root();

        // build up an arbitrary hierarchy to print out
        let left = root.child(Some("left".to_string()));
        let right = root.child(Some("right".to_string()));
        let mut sub = Vec::new();
        for n in 0..4 {
            let lsub = left.child(Some(format!("sub {n}")));
            let rsub = right.child(Some(format!("sub {n}")));

            match n {
                3 => {
                    sub.push(lsub.child(Some("deep".to_string())));
                }
                2 => {
                    sub.push(rsub.child(Some("deep".to_string())));
                }
                _ => {}
            }
            sub.push(lsub);
            sub.push(rsub);
        }

        right.print(true);
    }
}
