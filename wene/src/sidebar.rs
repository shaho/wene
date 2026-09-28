//! Sidebar: the Finder-style folder tree at the side of the browse
//! window. Two sections, Favorites and Local. The disclosure triangle
//! opens a folder without loading it; clicking the row loads that
//! folder into the grid (one level only, cmd-O stays recursive).
//!
//! Folders live in a flat arena. NSOutlineView needs one object per
//! item, so every arena slot is handed out as an NSNumber holding its
//! index. Slots are never reused, so a stale index can never point at
//! the wrong folder.

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{define_class, msg_send, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSControlTextEditingDelegate, NSDragOperation, NSDraggingInfo, NSEvent, NSEventModifierFlags,
    NSImage, NSImageView, NSMenu, NSMenuDelegate, NSMenuItem, NSOutlineView, NSOutlineViewDataSource,
    NSOutlineViewDelegate, NSOutlineViewDropOnItemIndex, NSPasteboardTypeFileURL, NSScrollView,
    NSTableCellView, NSTableColumn, NSTableViewRowSizeStyle, NSTableViewStyle, NSTextField, NSView,
    NSWorkspace,
};
use objc2_core_foundation::{CGPoint, CGSize};
use objc2_foundation::{
    ns_string, NSArray, NSIndexSet, NSInteger, NSNotification, NSNumber, NSObject,
    NSObjectProtocol, NSRect, NSString, NSURL, NSUserDefaults,
};

use crate::{e2e, AppDelegate};
use wene_core::transfer::Transfer;

/// Width of the sidebar pane, and the limits the split view allows.
pub const WIDTH: f64 = 200.0;
pub const MIN_WIDTH: f64 = 150.0;
pub const MAX_WIDTH: f64 = 340.0;

const ROW_H: f64 = 24.0;

/// One row of the tree: a section header or a folder.
struct Node {
    path: PathBuf,
    name: String,
    section: bool,
    /// Sub-folders, read the first time the row needs them.
    children: Option<Vec<usize>>,
    /// Whether the folder really holds a sub-folder, so rows without
    /// one never grow a triangle.
    expandable: Option<bool>,
}

pub struct SidebarIvars {
    nodes: RefCell<Vec<Node>>,
    /// Arena slots of the two section rows.
    favorites_root: Cell<usize>,
    local_root: Cell<usize>,
    favorites: RefCell<Vec<PathBuf>>,
    outline: OnceCell<Retained<NSOutlineView>>,
    delegate: OnceCell<Retained<AppDelegate>>,
    icons: RefCell<HashMap<PathBuf, Retained<NSImage>>>,
    /// True while the app moves the highlight to follow the grid, so
    /// the selection callback does not scan the folder again.
    syncing: Cell<bool>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "WeneSidebar"]
    #[ivars = SidebarIvars]
    pub struct Sidebar;

    unsafe impl NSObjectProtocol for Sidebar {}

    unsafe impl NSOutlineViewDataSource for Sidebar {
        #[unsafe(method(outlineView:numberOfChildrenOfItem:))]
        fn number_of_children(
            &self,
            _outline: &NSOutlineView,
            item: Option<&AnyObject>,
        ) -> isize {
            match index_of(item) {
                None => 2,
                Some(index) => {
                    self.load_children(index);
                    self.with_node(index, |node| {
                        node.children.as_ref().map(|c| c.len()).unwrap_or(0) as isize
                    })
                }
            }
        }

        #[unsafe(method_id(outlineView:child:ofItem:))]
        #[unsafe(method_family = none)]
        fn child_of_item(
            &self,
            _outline: &NSOutlineView,
            index: isize,
            item: Option<&AnyObject>,
        ) -> Retained<AnyObject> {
            let slot = match index_of(item) {
                None => {
                    if index == 0 {
                        self.ivars().favorites_root.get()
                    } else {
                        self.ivars().local_root.get()
                    }
                }
                Some(parent) => {
                    self.load_children(parent);
                    // usize::MAX stands for "no such row": returning
                    // the parent would nest it inside itself.
                    self.with_node(parent, |node| {
                        node.children
                            .as_ref()
                            .and_then(|c| c.get(index as usize).copied())
                            .unwrap_or(usize::MAX)
                    })
                }
            };
            item_for(slot)
        }

        #[unsafe(method(outlineView:isItemExpandable:))]
        fn is_item_expandable(&self, _outline: &NSOutlineView, item: &AnyObject) -> bool {
            self.expandable(item)
        }

        // A drop is only ever on a folder row, never between rows:
        // the tree has no order to insert into.
        #[unsafe(method(outlineView:validateDrop:proposedItem:proposedChildIndex:))]
        fn validate_drop(
            &self,
            outline: &NSOutlineView,
            info: &ProtocolObject<dyn NSDraggingInfo>,
            item: Option<&AnyObject>,
            index: NSInteger,
        ) -> NSDragOperation {
            let Some(target) = self.drop_target(item) else {
                return NSDragOperation::None;
            };
            if !drop_allowed(&target, &dropped_paths(info)) {
                return NSDragOperation::None;
            }
            if index != NSOutlineViewDropOnItemIndex as NSInteger {
                // Retarget a drop between rows onto the row itself.
                unsafe {
                    outline.setDropItem_dropChildIndex(
                        item,
                        NSOutlineViewDropOnItemIndex as NSInteger,
                    )
                };
            }
            drop_kind()
        }

        #[unsafe(method(outlineView:acceptDrop:item:childIndex:))]
        fn accept_drop(
            &self,
            _outline: &NSOutlineView,
            info: &ProtocolObject<dyn NSDraggingInfo>,
            item: Option<&AnyObject>,
            _index: NSInteger,
        ) -> bool {
            self.take_drop(info, item)
        }
    }

    // NSOutlineViewDelegate builds on this one; the sidebar edits no
    // text, so it stays empty.
    unsafe impl NSControlTextEditingDelegate for Sidebar {}

    unsafe impl NSOutlineViewDelegate for Sidebar {
        #[unsafe(method(outlineView:isGroupItem:))]
        fn is_group_item(&self, _outline: &NSOutlineView, item: &AnyObject) -> bool {
            index_of(Some(item)).is_some_and(|index| self.with_node(index, |node| node.section))
        }

        #[unsafe(method(outlineView:shouldSelectItem:))]
        fn should_select_item(&self, _outline: &NSOutlineView, item: &AnyObject) -> bool {
            index_of(Some(item)).is_some_and(|index| self.with_node(index, |node| !node.section))
        }

        #[unsafe(method_id(outlineView:viewForTableColumn:item:))]
        #[unsafe(method_family = none)]
        fn view_for_item(
            &self,
            _outline: &NSOutlineView,
            _column: Option<&NSTableColumn>,
            item: Option<&AnyObject>,
        ) -> Option<Retained<NSView>> {
            self.row_view_for(item)
        }

        #[unsafe(method(outlineViewSelectionDidChange:))]
        fn selection_did_change(&self, _notification: &NSNotification) {
            if self.ivars().syncing.get() {
                return;
            }
            self.open_selected();
        }
    }

    unsafe impl NSMenuDelegate for Sidebar {
        // The right-click menu is built for the row under the
        // pointer. Section headers are not folders, so they get none.
        #[unsafe(method(menuNeedsUpdate:))]
        fn menu_needs_update(&self, menu: &NSMenu) {
            menu.removeAllItems();
            let Some(index) = self.clicked_index() else { return };
            self.fill_context_menu(menu, index);
        }
    }

    impl Sidebar {
        #[unsafe(method(rowClicked:))]
        fn row_clicked(&self, _sender: Option<&AnyObject>) {
            self.open_selected();
        }

        #[unsafe(method(addFavorite:))]
        fn add_favorite_action(&self, _sender: Option<&AnyObject>) {
            let Some(index) = self.clicked_index() else { return };
            self.add_favorite(&self.path_of(index));
        }

        #[unsafe(method(removeFavorite:))]
        fn remove_favorite_action(&self, _sender: Option<&AnyObject>) {
            let Some(index) = self.clicked_index() else { return };
            self.remove_favorite(&self.path_of(index));
        }

        #[unsafe(method(scanRecursive:))]
        fn scan_recursive_action(&self, _sender: Option<&AnyObject>) {
            let Some(index) = self.clicked_index() else { return };
            self.scan_recursive(index);
        }

        #[unsafe(method(openInFinder:))]
        fn open_in_finder_action(&self, _sender: Option<&AnyObject>) {
            let Some(index) = self.clicked_index() else { return };
            self.open_in_finder(index);
        }

        #[unsafe(method(volumesChanged:))]
        fn volumes_changed(&self, _notification: Option<&NSNotification>) {
            self.reload_section(self.ivars().local_root.get());
        }
    }
);

impl Sidebar {
    /// Build the tree and the scroll view that holds it. The caller
    /// puts the scroll view in the split view's sidebar item.
    pub fn new(
        mtm: MainThreadMarker,
        delegate: &Retained<AppDelegate>,
    ) -> (Retained<Self>, Retained<NSScrollView>) {
        let favorites = load_favorites();
        let mut nodes = Vec::new();
        nodes.push(section_node("Favorites"));
        nodes.push(section_node("Local"));

        let this = Self::alloc(mtm).set_ivars(SidebarIvars {
            nodes: RefCell::new(nodes),
            favorites_root: Cell::new(0),
            local_root: Cell::new(1),
            favorites: RefCell::new(favorites),
            outline: OnceCell::new(),
            delegate: OnceCell::new(),
            icons: RefCell::new(HashMap::new()),
            syncing: Cell::new(false),
        });
        let this: Retained<Self> = unsafe { msg_send![super(this), init] };
        let _ = this.ivars().delegate.set(delegate.clone());

        let frame = NSRect::new(CGPoint::new(0.0, 0.0), CGSize::new(WIDTH, 400.0));
        let outline = NSOutlineView::initWithFrame(NSOutlineView::alloc(mtm), frame);
        let column = NSTableColumn::initWithIdentifier(NSTableColumn::alloc(mtm), ns_string!("name"));
        column.setWidth(WIDTH);
        outline.addTableColumn(&column);
        unsafe { outline.setOutlineTableColumn(Some(&column)) };
        outline.setHeaderView(None);
        outline.setStyle(NSTableViewStyle::SourceList);
        outline.setRowSizeStyle(NSTableViewRowSizeStyle::Default);
        outline.setIndentationPerLevel(14.0);
        // Section headers stay with their rows, never float over them.
        outline.setFloatsGroupRows(false);
        unsafe {
            outline.setDataSource(Some(ProtocolObject::from_ref(&*this)));
            outline.setDelegate(Some(ProtocolObject::from_ref(&*this)));
            // Clicks come through the action, key presses through the
            // selection callback. Both end in open_selected.
            outline.setTarget(Some(&this));
            outline.setAction(Some(objc2::sel!(rowClicked:)));
        }

        // Images dragged out of the grid, or in from Finder, land
        // on a folder row.
        unsafe {
            outline.registerForDraggedTypes(&NSArray::from_slice(&[NSPasteboardTypeFileURL]))
        };

        let menu = NSMenu::new(mtm);
        menu.setDelegate(Some(ProtocolObject::from_ref(&*this)));
        unsafe { outline.setMenu(Some(&menu)) };

        let scroll = NSScrollView::initWithFrame(NSScrollView::alloc(mtm), frame);
        scroll.setHasVerticalScroller(true);
        scroll.setDrawsBackground(false);
        scroll.setDocumentView(Some(&outline));

        let _ = this.ivars().outline.set(outline.clone());
        // Both sections start open, like Finder.
        unsafe {
            outline.expandItem(Some(&item_for(this.ivars().favorites_root.get())));
            outline.expandItem(Some(&item_for(this.ivars().local_root.get())));
        }

        // Plugging or ejecting a disk refreshes the Local section.
        unsafe {
            let center = NSWorkspace::sharedWorkspace().notificationCenter();
            for name in [
                objc2_app_kit::NSWorkspaceDidMountNotification,
                objc2_app_kit::NSWorkspaceDidUnmountNotification,
            ] {
                center.addObserver_selector_name_object(
                    &this,
                    objc2::sel!(volumesChanged:),
                    Some(name),
                    None,
                );
            }
        }

        (this, scroll)
    }

    /// Whether a row gets a disclosure triangle. Sections always do;
    /// a folder only when it really holds a sub-folder.
    fn expandable(&self, item: &AnyObject) -> bool {
        let Some(index) = index_of(Some(item)) else { return false };
        if self.with_node(index, |node| node.section) {
            return true;
        }
        if let Some(known) = self.with_node(index, |node| node.expandable) {
            return known;
        }
        let expandable = has_subfolder(&self.path_of(index));
        self.ivars().nodes.borrow_mut()[index].expandable = Some(expandable);
        expandable
    }

    fn row_view_for(&self, item: Option<&AnyObject>) -> Option<Retained<NSView>> {
        let index = index_of(item)?;
        let (name, section) = self.with_node(index, |node| (node.name.clone(), node.section));
        let icon = (!section).then(|| self.icon(index));
        Some(row_view(self.mtm(), &name, icon.as_deref()))
    }

    /// Load the highlighted folder into the grid. A sidebar click
    /// lists that folder only; cmd-O stays recursive, so clicking the
    /// folder cmd-O opened narrows it back to one level.
    fn open_selected(&self) {
        let Some(path) = self.selected_path() else { return };
        let Some(delegate) = self.ivars().delegate.get() else { return };
        if delegate.current_scan() == Some((path.clone(), false)) {
            return;
        }
        delegate.scan_root(path, false);
    }

    fn mtm(&self) -> MainThreadMarker {
        MainThreadMarker::new().expect("sidebar runs on the main thread")
    }

    fn outline(&self) -> &NSOutlineView {
        self.ivars().outline.get().expect("outline view is set")
    }

    /// Read one arena slot. An index AppKit hands back that the arena
    /// never issued reads as an empty row instead of panicking.
    fn with_node<T>(&self, index: usize, f: impl FnOnce(&Node) -> T) -> T {
        let nodes = self.ivars().nodes.borrow();
        match nodes.get(index) {
            Some(node) => f(node),
            None => f(&folder_node(PathBuf::new(), Some(String::new()))),
        }
    }

    /// Build the right-click menu for one row. Split out from the
    /// menu delegate so the e2e harness can build the same menu for a
    /// row it names itself.
    pub fn fill_context_menu(&self, menu: &NSMenu, index: usize) {
        if self.with_node(index, |node| node.section) {
            return;
        }
        let mtm = self.mtm();
        let path = self.path_of(index);
        let favorite = self.ivars().favorites.borrow().contains(&path);
        let entries: [Option<(&str, objc2::runtime::Sel)>; 5] = [
            Some(("Include subfolders", objc2::sel!(scanRecursive:))),
            None,
            Some(if favorite {
                ("Remove from favorites", objc2::sel!(removeFavorite:))
            } else {
                ("Add to favorites", objc2::sel!(addFavorite:))
            }),
            None,
            Some(("Open in Finder", objc2::sel!(openInFinder:))),
        ];
        for entry in entries {
            let Some((title, action)) = entry else {
                menu.addItem(&NSMenuItem::separatorItem(mtm));
                continue;
            };
            let item = unsafe {
                NSMenuItem::initWithTitle_action_keyEquivalent(
                    NSMenuItem::alloc(mtm),
                    &NSString::from_str(title),
                    Some(action),
                    ns_string!(""),
                )
            };
            unsafe { item.setTarget(Some(self)) };
            menu.addItem(&item);
        }
    }

    /// Load the row's folder and everything under it, the way cmd-O
    /// does, and move the highlight onto it.
    fn scan_recursive(&self, index: usize) {
        let path = self.path_of(index);
        if let Some(delegate) = self.ivars().delegate.get() {
            delegate.scan_root(path, true);
        }
    }

    fn open_in_finder(&self, index: usize) {
        let url = NSURL::fileURLWithPath(&NSString::from_str(
            &self.path_of(index).to_string_lossy(),
        ));
        NSWorkspace::sharedWorkspace().openURL(&url);
    }

    /// File the dropped images into the row's folder. A plain drop
    /// moves, the way Finder does inside one disk; option copies.
    fn take_drop(
        &self,
        info: &ProtocolObject<dyn NSDraggingInfo>,
        item: Option<&AnyObject>,
    ) -> bool {
        let Some(target) = self.drop_target(item) else { return false };
        let paths = dropped_paths(info);
        let Some(delegate) = self.ivars().delegate.get() else { return false };
        if paths.is_empty() {
            return false;
        }
        let kind = if drop_kind() == NSDragOperation::Copy {
            Transfer::Copy
        } else {
            Transfer::Move
        };
        delegate.transfer_paths(paths, target, kind);
        true
    }

    /// The folder a drop would land in: a folder row, never a section
    /// header and never the gap below the last row.
    fn drop_target(&self, item: Option<&AnyObject>) -> Option<PathBuf> {
        let index = index_of(item)?;
        self.with_node(index, |node| (!node.section).then(|| node.path.clone()))
    }

    fn path_of(&self, index: usize) -> PathBuf {
        self.with_node(index, |node| node.path.clone())
    }

    /// Read a row's sub-folders once, and keep them.
    fn load_children(&self, index: usize) {
        if self.with_node(index, |node| node.children.is_some()) {
            return;
        }
        let (path, section) = self.with_node(index, |node| (node.path.clone(), node.section));
        let rows: Vec<(PathBuf, Option<String>)> = if section {
            if index == self.ivars().favorites_root.get() {
                self.ivars()
                    .favorites
                    .borrow()
                    .iter()
                    .map(|p| (p.clone(), None))
                    .collect()
            } else {
                volumes()
            }
        } else {
            subfolders(&path).into_iter().map(|p| (p, None)).collect()
        };
        let mut nodes = self.ivars().nodes.borrow_mut();
        let mut children = Vec::with_capacity(rows.len());
        for (path, name) in rows {
            children.push(nodes.len());
            nodes.push(folder_node(path, name));
        }
        nodes[index].children = Some(children);
    }

    /// Drop a section's rows and read them again.
    fn reload_section(&self, index: usize) {
        self.ivars().nodes.borrow_mut()[index].children = None;
        self.load_children(index);
        let item = item_for(index);
        unsafe { self.outline().reloadItem_reloadChildren(Some(&item), true) };
    }

    fn icon(&self, index: usize) -> Retained<NSImage> {
        let path = self.path_of(index);
        if let Some(icon) = self.ivars().icons.borrow().get(&path) {
            return icon.clone();
        }
        let icon = NSWorkspace::sharedWorkspace()
            .iconForFile(&NSString::from_str(&path.to_string_lossy()));
        icon.setSize(CGSize::new(16.0, 16.0));
        self.ivars()
            .icons
            .borrow_mut()
            .insert(path, icon.clone());
        icon
    }

    fn clicked_index(&self) -> Option<usize> {
        let row = self.outline().clickedRow();
        if row < 0 {
            return None;
        }
        index_of(self.outline().itemAtRow(row).as_deref())
    }

    pub fn selected_path(&self) -> Option<PathBuf> {
        let row = self.outline().selectedRow();
        if row < 0 {
            return None;
        }
        let index = index_of(self.outline().itemAtRow(row).as_deref())?;
        let (path, section) = self.with_node(index, |node| (node.path.clone(), node.section));
        (!section).then_some(path)
    }

    // ---- favorites ----

    pub fn add_favorite(&self, path: &Path) {
        {
            let mut favorites = self.ivars().favorites.borrow_mut();
            if favorites.contains(&path.to_path_buf()) {
                return;
            }
            favorites.push(path.to_path_buf());
        }
        self.save_favorites();
        self.reload_section(self.ivars().favorites_root.get());
    }

    pub fn remove_favorite(&self, path: &Path) {
        self.ivars().favorites.borrow_mut().retain(|p| p != path);
        self.save_favorites();
        self.reload_section(self.ivars().favorites_root.get());
    }

    fn save_favorites(&self) {
        if e2e::enabled() {
            return;
        }
        let strings: Vec<Retained<NSString>> = self
            .ivars()
            .favorites
            .borrow()
            .iter()
            .map(|p| NSString::from_str(&p.to_string_lossy()))
            .collect();
        let refs: Vec<&NSString> = strings.iter().map(|s| &**s).collect();
        let array = objc2_foundation::NSArray::from_slice(&refs);
        unsafe {
            NSUserDefaults::standardUserDefaults()
                .setObject_forKey(Some(&array), ns_string!("favorites"));
        }
    }

    // ---- following the grid ----

    /// Open the tree down to `path`, highlight it, and scroll it into
    /// view. The highlighted row must never disagree with the grid.
    pub fn reveal(&self, path: &Path) {
        let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        // Expanding rows can move the highlight, so the guard covers
        // the whole walk: none of it may scan a folder again.
        self.ivars().syncing.set(true);
        self.walk_to(&target);
        self.ivars().syncing.set(false);
    }

    fn walk_to(&self, target: &Path) {
        let Some((mut index, base)) = self.best_root(target) else {
            return unsafe { self.outline().deselectAll(None) };
        };
        if let Ok(rest) = target.strip_prefix(&base) {
            for component in rest.components() {
                let name = component.as_os_str();
                unsafe { self.outline().expandItem(Some(&item_for(index))) };
                self.load_children(index);
                let next = self.with_node(index, |node| node.children.clone()).and_then(|kids| {
                    kids.into_iter()
                        .find(|kid| self.with_node(*kid, |n| n.path.file_name() == Some(name)))
                });
                match next {
                    Some(next) => index = next,
                    // The tree leaves out dot-folders and packages, so
                    // some folders have no row. Highlighting an
                    // ancestor would disagree with the grid.
                    None => return unsafe { self.outline().deselectAll(None) },
                }
            }
        }
        let row = unsafe { self.outline().rowForItem(Some(&item_for(index))) };
        if row < 0 {
            return unsafe { self.outline().deselectAll(None) };
        }
        self.outline()
            .selectRowIndexes_byExtendingSelection(&NSIndexSet::indexSetWithIndex(row as usize), false);
        self.outline().scrollRowToVisible(row);
    }

    /// The row to start walking from: the favorite or the volume that
    /// holds `target`, taking the deepest match.
    fn best_root(&self, target: &Path) -> Option<(usize, PathBuf)> {
        let mut best: Option<(usize, PathBuf)> = None;
        for section in [
            self.ivars().favorites_root.get(),
            self.ivars().local_root.get(),
        ] {
            self.load_children(section);
            let children = self
                .with_node(section, |node| node.children.clone())
                .unwrap_or_default();
            for child in children {
                let path = self.path_of(child);
                let Ok(real) = std::fs::canonicalize(&path) else { continue };
                if !target.starts_with(&real) {
                    continue;
                }
                let deeper = best
                    .as_ref()
                    .map(|(_, current)| real.components().count() > current.components().count())
                    .unwrap_or(true);
                if deeper {
                    best = Some((child, real));
                }
            }
        }
        best
    }

    // ---- e2e accessors ----

    pub fn e2e_click(&self, path: &Path) {
        self.reveal(path);
        if let (Some(path), Some(delegate)) = (self.selected_path(), self.ivars().delegate.get()) {
            delegate.scan_root(path, false);
        }
    }

    /// Titles of the right-click menu for the row that holds `path`.
    pub fn e2e_context_menu(&self, path: &Path) -> Vec<String> {
        let index = {
            let nodes = self.ivars().nodes.borrow();
            nodes.iter().position(|node| !node.section && node.path == path)
        };
        let menu = NSMenu::new(self.mtm());
        if let Some(index) = index {
            self.fill_context_menu(&menu, index);
        }
        menu.itemArray().iter().map(|item| item.title().to_string()).collect()
    }

    pub fn e2e_favorites(&self) -> Vec<PathBuf> {
        self.ivars().favorites.borrow().clone()
    }
}

/// The images a drop carries, as paths. Anything that is not a file
/// URL is ignored.
fn dropped_paths(info: &ProtocolObject<dyn NSDraggingInfo>) -> Vec<PathBuf> {
    let pasteboard = info.draggingPasteboard();
    let Some(items) = pasteboard.pasteboardItems() else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| unsafe { item.stringForType(NSPasteboardTypeFileURL) })
        .filter_map(|text| NSURL::URLWithString(&text))
        .filter_map(|url| url.path())
        .map(|path| PathBuf::from(path.to_string()))
        .collect()
}

/// Whether a drop on `target` is worth taking: at least one image has
/// to come from somewhere else. A drop on the folder the images
/// already sit in would do nothing.
fn drop_allowed(target: &Path, paths: &[PathBuf]) -> bool {
    paths.iter().any(|path| path.parent() != Some(target))
}

/// A drop moves, the way dragging inside one disk does in Finder, and
/// copies while the option key is down.
fn drop_kind() -> NSDragOperation {
    let held = NSEvent::modifierFlags_class();
    if held.contains(NSEventModifierFlags::Option) {
        NSDragOperation::Copy
    } else {
        NSDragOperation::Move
    }
}

/// The index an outline item stands for.
fn index_of(item: Option<&AnyObject>) -> Option<usize> {
    item?.downcast_ref::<NSNumber>().map(|n| n.as_usize())
}

/// The outline item for an arena slot.
fn item_for(index: usize) -> Retained<AnyObject> {
    unsafe { Retained::cast_unchecked(NSNumber::new_usize(index)) }
}

fn section_node(name: &str) -> Node {
    Node {
        path: PathBuf::new(),
        name: name.to_string(),
        section: true,
        children: None,
        expandable: Some(true),
    }
}

/// `display` overrides the folder's own name, which the startup disk
/// needs: its path is `/` but its name is the volume's.
fn folder_node(path: PathBuf, display: Option<String>) -> Node {
    let name = display.unwrap_or_else(|| {
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string_lossy().into_owned())
    });
    Node {
        path,
        name,
        section: false,
        children: None,
        expandable: None,
    }
}

/// One sidebar row: the folder icon and its name.
fn row_view(mtm: MainThreadMarker, name: &str, icon: Option<&NSImage>) -> Retained<NSView> {
    let cell = NSTableCellView::initWithFrame(
        NSTableCellView::alloc(mtm),
        NSRect::new(CGPoint::new(0.0, 0.0), CGSize::new(WIDTH, ROW_H)),
    );
    let text_x = if icon.is_some() { 24.0 } else { 4.0 };
    if let Some(icon) = icon {
        let view = NSImageView::initWithFrame(
            NSImageView::alloc(mtm),
            NSRect::new(CGPoint::new(2.0, 4.0), CGSize::new(16.0, 16.0)),
        );
        view.setImage(Some(icon));
        cell.addSubview(&view);
        unsafe { cell.setImageView(Some(&view)) };
    }
    let field = NSTextField::labelWithString(&NSString::from_str(name), mtm);
    field.setFrame(NSRect::new(
        CGPoint::new(text_x, 3.0),
        CGSize::new(WIDTH - text_x - 4.0, 17.0),
    ));
    field.setAutoresizingMask(objc2_app_kit::NSAutoresizingMaskOptions::ViewWidthSizable);
    field.setFont(Some(&objc2_app_kit::NSFont::systemFontOfSize(13.0)));
    cell.addSubview(&field);
    unsafe { cell.setTextField(Some(&field)) };
    Retained::into_super(cell)
}

/// Favorites from preferences; first run seeds Pictures, Desktop, and
/// the home folder.
fn load_favorites() -> Vec<PathBuf> {
    let home = std::env::var("HOME").map(PathBuf::from).unwrap_or_default();
    let seed = || vec![home.join("Pictures"), home.join("Desktop"), home.clone()];
    if e2e::enabled() {
        return seed();
    }
    let stored = NSUserDefaults::standardUserDefaults().stringArrayForKey(ns_string!("favorites"));
    // A stored list wins even when it is empty: the seeds are for a
    // first run, not for someone who removed every favorite.
    match stored {
        Some(array) => array.iter().map(|s| PathBuf::from(s.to_string())).collect(),
        None => seed(),
    }
}

/// Mounted volumes, each with its name. The startup disk keeps the
/// path `/` so file paths match it, and shows its volume name.
fn volumes() -> Vec<(PathBuf, Option<String>)> {
    let Ok(entries) = std::fs::read_dir("/Volumes") else {
        return vec![(PathBuf::from("/"), Some("Macintosh HD".to_string()))];
    };
    let mut names: Vec<String> = entries
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !n.starts_with('.'))
        .filter(|n| Path::new("/Volumes").join(n).is_dir())
        .collect();
    names.sort_by(|a, b| wene_core::natural_str_cmp(a, b));
    let mut volumes: Vec<(PathBuf, Option<String>)> = Vec::with_capacity(names.len());
    let mut has_startup = false;
    for name in names {
        let mounted = PathBuf::from("/Volumes").join(&name);
        let startup = std::fs::canonicalize(&mounted).is_ok_and(|real| real == Path::new("/"));
        has_startup |= startup;
        let path = if startup { PathBuf::from("/") } else { mounted };
        volumes.push((path, Some(name)));
    }
    if !has_startup {
        volumes.insert(0, (PathBuf::from("/"), Some("Macintosh HD".to_string())));
    }
    volumes
}

/// Direct sub-folders, hidden ones left out, in natural name order.
fn subfolders(path: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(path) else {
        return Vec::new();
    };
    let mut folders: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| !p.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.')))
        .filter(|p| p.is_dir())
        .filter(|p| !is_package(p))
        .collect();
    folders.sort_by(|a, b| {
        wene_core::natural_str_cmp(
            &a.file_name().unwrap_or_default().to_string_lossy(),
            &b.file_name().unwrap_or_default().to_string_lossy(),
        )
    });
    folders
}

/// Bundles (photo libraries, apps) open as one thing in Finder, so
/// the tree leaves them out.
fn is_package(path: &Path) -> bool {
    NSWorkspace::sharedWorkspace().isFilePackageAtPath(&NSString::from_str(&path.to_string_lossy()))
}

/// Whether a folder holds at least one sub-folder. Stops at the first
/// one it finds, so a huge folder costs little.
fn has_subfolder(path: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(path) else {
        return false;
    };
    entries.flatten().any(|e| {
        !e.file_name().to_string_lossy().starts_with('.')
            && e.path().is_dir()
            && !is_package(&e.path())
    })
}

#[cfg(test)]
mod tests {
    use super::drop_allowed;
    use std::path::{Path, PathBuf};

    #[test]
    fn a_drop_needs_an_image_from_another_folder() {
        let target = Path::new("/pics/keepers");
        let inside = vec![PathBuf::from("/pics/keepers/a.heic")];
        let outside = vec![PathBuf::from("/pics/a.heic")];
        let mixed = vec![
            PathBuf::from("/pics/keepers/a.heic"),
            PathBuf::from("/pics/b.heic"),
        ];
        assert!(!drop_allowed(target, &inside));
        assert!(!drop_allowed(target, &[]));
        assert!(drop_allowed(target, &outside));
        assert!(drop_allowed(target, &mixed));
    }
}
