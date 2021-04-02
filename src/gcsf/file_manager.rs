use super::{File, FileId};
use drive3;
use failure::{err_msg, Error};
use fuser::{FileAttr, FileType};
use id_tree::InsertBehavior::*;
use id_tree::MoveBehavior::*;
use id_tree::RemoveBehavior::*;
use id_tree::{Node, NodeId, Tree, TreeBuilder};
use std::collections::{HashMap, LinkedList};
use std::fmt;
use std::time::{Duration, SystemTime};
use DriveFacade;

pub type Inode = u64;
pub type FileHandle = u64;
pub type DriveId = String;

const ROOT_INODE: Inode = 1;
const TRASH_INODE: Inode = 2;
const SHARED_INODE: Inode = 3;
const ORPHANS_INODE: Inode = 4;

macro_rules! unwrap_or_continue {
    ($res:expr) => {
        match $res {
            Some(val) => val,
            None => {
                warn!("unwrap_or_continue!(): skipped.");
                continue;
            }
        }
    };
}

/// Manages files locally and uses a DriveFacade in order to communicate with Google Drive and to ensure consistency between the local and remote state.
pub struct FileManager {
    /// A representation of the file tree. Each tree node stores the inode of the corresponding file.
    tree: Tree<Inode>,

    /// Maps inodes to the corresponding files.
    pub files: HashMap<Inode, File>,

    /// Maps inodes to corresponding node ids that `tree` uses.
    pub node_ids: HashMap<Inode, NodeId>,

    /// Maps Google Drive ids (i.e strings) to corresponding inodes.
    pub drive_ids: HashMap<DriveId, Inode>,

    /// A `DriveFacade` is used in order to communicate with the Google Drive API.
    pub df: DriveFacade,

    /// The last timestamp when the file manager asked Google Drive for remote changes.
    pub last_sync: SystemTime,

    /// Specifies how much time is needed to pass since `last_sync` for a new sync to be performed.
    pub sync_interval: Duration,

    /// Rename duplicate files if enabled.
    pub rename_identical_files: bool,

    /// Add an extension to special files (docs, presentations, sheets, drawings, sites).
    /// e.g. "#.ods" for spreadsheets.
    pub add_extensions_to_special_files: bool,

    /// If enabled, deleting files will remove them permanently instead of moving them to Trash.
    /// Deleting trashed files always removes them permanently.
    pub skip_trash: bool,

    /// New inodes are assigned incrementally. This keeps track of the last used inode.
    last_inode: Inode,

    /// New file handles are assigned incrementally. This keeps track of the last used file handle.
    last_fh: FileHandle,
}

impl FileManager {
    /// Creates a new FileManager with a specific `sync_interval` and an injected `DriveFacade`.
    /// Also populates the manager's file tree with files contained in "My Drive" and "Trash".
    pub fn with_drive_facade(
        rename_identical_files: bool,
        add_extensions_to_special_files: bool,
        skip_trash: bool,
        sync_interval: Duration,
        df: DriveFacade,
    ) -> Result<Self, Error> {
        let mut manager = FileManager {
            tree: TreeBuilder::new().with_node_capacity(500).build(),
            files: HashMap::new(),
            node_ids: HashMap::new(),
            drive_ids: HashMap::new(),
            last_sync: SystemTime::now(),
            rename_identical_files,
            add_extensions_to_special_files,
            skip_trash,
            sync_interval,
            df,
            last_inode: 4,
            last_fh: 3,
        };

        manager
            .populate()
            .map_err(|e| err_msg(format!("Could not populate file system:\n{}", e)))?;
        manager
            .populate_trash()
            .map_err(|e| err_msg(format!("Could not populate trash dir:\n{}", e)))?;
        Ok(manager)
    }

    /// Tries to retrieve recent changes from the `DriveFacade` and apply them locally in order to
    /// maintain data consistency. Fails early if not enough time has passed since the last sync.
    pub fn sync(&mut self) -> Result<(), Error> {
        if SystemTime::now().duration_since(self.last_sync).unwrap() < self.sync_interval {
            return Err(err_msg(
                "Not enough time has passed since last sync. Will do nothing.",
            ));
        }

        info!("Checking for changes and possibly applying them.");
        self.last_sync = SystemTime::now();

        for change in self
            .df
            .get_all_changes()?
            .into_iter()
            .filter(|change| change.file.is_some())
        {
            debug!("Processing a change from {:?}", &change.time);
            let id = FileId::DriveId(change.file_id.unwrap());
            let drive_f = change.file.unwrap();

            // New file. Create it locally
            if !self.contains(&id) {
                debug!("New file. Create it locally");
                let f = File::from_drive_file(
                    self.next_available_inode(),
                    drive_f.clone(),
                    self.add_extensions_to_special_files,
                );
                debug!("newly created file: {:#?}", &f);

                let parent = f.drive_parent().unwrap();
                debug!("drive parent: {:#?}", &parent);
                self.add_file_locally(f, Some(FileId::DriveId(parent)))?;
                debug!("self.add_file_locally() finished");
            }

            // Trashed file. Move it to trash locally
            if Some(true) == drive_f.trashed {
                debug!("Trashed file. Move it to trash locally");
                let result = self.move_file_to_trash(&id, false);
                if result.is_err() {
                    error!("Could not move to trash: {:?}", result)
                }
                continue;
            }

            // Removed file. Remove it locally.
            if let Some(true) = change.removed {
                debug!("Removed file. Remove it locally.");
                let result = self.delete_locally(&id);
                if result.is_err() {
                    error!("Could not delete locally: {:?}", result)
                }
                continue;
            }

            // Anything else: reconstruct the file locally and move it under its parent.
            debug!("Anything else: reconstruct the file locally and move it under its parent.");
            let new_parent = {
                let add_extension = self.add_extensions_to_special_files;
                let f = unwrap_or_continue!(self.get_mut_file(&id));
                *f = File::from_drive_file(f.inode(), drive_f.clone(), add_extension);
                FileId::DriveId(f.drive_parent().unwrap())
            };
            let result = self.move_locally(&id, &new_parent);
            if result.is_err() {
                error!("Could not move locally: {:?}", result)
            }
        }

        Ok(())
    }

    /// Creates special dirs: root (.), "Shared with me", "Orphans".
    fn create_special_dirs(&mut self) -> Result<(), Error> {
        let root = self.new_root_file();
        let shared = self.new_special_dir("Shared with me", Some(SHARED_INODE));
        let orphans = self.new_special_dir("Orphans", Some(ORPHANS_INODE));
        self.add_file_locally(root, None)?;
        self.add_file_locally(shared, Some(FileId::Inode(ROOT_INODE)))?;
        self.add_file_locally(orphans, Some(FileId::Inode(ROOT_INODE)))?;
        Ok(())
    }

    /// Retrieves all files and directories shown in "My Drive" and "Shared with me" and adds them locally.
    fn populate(&mut self) -> Result<(), Error> {
        self.create_special_dirs()?;

        let drive_files = self
            .df
            .get_all_files(/*parents:*/ None, /*trashed:*/ Some(false))?
            .iter()
            .map(|drive_file| {
                File::from_drive_file(
                    self.next_available_inode(),
                    drive_file.clone(),
                    self.add_extensions_to_special_files,
                )
            })
            .collect::<LinkedList<_>>();

        // Add everything to "Orphans" dir initially.
        for file in drive_files {
            info!("asdf: {}", &file.name());
            self.add_file_locally(file, Some(FileId::Inode(ORPHANS_INODE)))?;
        }

        // Find the proper parent of every file somewhere in the flat hierarchy.
        let moves = self
            .files
            .iter()
            .filter(|(_, file)| file.drive_parent().is_some())
            .map(|(inode, file)| {
                (
                    FileId::Inode(*inode),
                    FileId::DriveId(file.drive_parent().unwrap()),
                )
            })
            .filter(|(_, parent)| self.contains(parent))
            .collect::<LinkedList<_>>();

        // Move every file under its proper parent.
        moves.iter().for_each(|(inode, parent)| {
            if let Err(e) = self.move_locally(inode, parent) {
                error!("{}", e);
            }
        });

        Ok(())
    }

    /// Retrieves all trashed files and directories and adds them locally in a special directory.
    fn populate_trash(&mut self) -> Result<(), Error> {
        let root_id = self.df.root_id()?.to_string();
        let trash = self.new_special_dir("Trash", Some(TRASH_INODE));
        self.add_file_locally(trash.clone(), Some(FileId::DriveId(root_id)))?;

        for drive_file in self
            .df
            .get_all_files(/*parents:*/ None, /*trashed:*/ Some(true))?
        {
            let file = File::from_drive_file(
                self.next_available_inode(),
                drive_file,
                self.add_extensions_to_special_files,
            );
            self.add_file_locally(file, Some(FileId::Inode(trash.inode())))?;
        }

        Ok(())
    }

    /// Creates a new File which represents the root directory.
    /// If possible, it fills in the exact DriveId.
    /// If not, it keeps using "root" as a placeholder id.
    fn new_root_file(&mut self) -> File {
        let mut drive_file = drive3::File::default();

        let fallback_id = String::from("root");
        let root_id = self.df.root_id().unwrap_or(&fallback_id);
        drive_file.id = Some(root_id.to_string());

        File {
            name: String::from("."),
            attr: FileAttr {
                ino: ROOT_INODE,
                size: 512,
                blocks: 1,
                blksize: 0,
                padding: 0,
                atime: SystemTime::now(),
                mtime: SystemTime::now(),
                ctime: SystemTime::now(),
                crtime: SystemTime::now(),
                kind: FileType::Directory,
                perm: 0o755,
                nlink: 2,
                uid: 0,
                gid: 0,
                rdev: 0,
                flags: 0,
            },
            identical_name_id: None,
            drive_file: Some(drive_file),
        }
    }

    /// Creates a new File struct which represents a directory that does not necessarily exist on Drive.
    fn new_special_dir(&mut self, name: &str, preferred_inode: Option<Inode>) -> File {
        File {
            name: name.to_string(),
            attr: FileAttr {
                ino: preferred_inode.unwrap_or_else(|| self.next_available_inode()),
                size: 512,
                blocks: 1,
                blksize: 0,
                padding: 0,
                atime: SystemTime::now(),
                mtime: SystemTime::now(),
                ctime: SystemTime::now(),
                crtime: SystemTime::now(),
                kind: FileType::Directory,
                perm: 0o755,
                nlink: 2,
                uid: 0,
                gid: 0,
                rdev: 0,
                flags: 0,
            },
            identical_name_id: None,
            drive_file: None,
        }
    }

    /// Returns the next unused inode.
    pub fn next_available_inode(&mut self) -> Inode {
        self.last_inode += 1;
        self.last_inode
    }

    /// Returns the next unused file handle.
    pub fn next_available_fh(&mut self) -> FileHandle {
        self.last_fh += 1;
        self.last_fh
    }

    /// Returns true if the file identified by a given id exists in the filesystem.
    pub fn contains(&self, file_id: &FileId) -> bool {
        match file_id {
            FileId::Inode(inode) => self.node_ids.contains_key(&inode),
            FileId::DriveId(drive_id) => self.drive_ids.contains_key(drive_id),
            FileId::NodeId(node_id) => self.tree.get(&node_id).is_ok(),
            pn @ FileId::ParentAndName { .. } => self.get_file(&pn).is_some(),
        }
    }

    /// Returns the NodeId of a file identified by a given id.
    /// The NodeId indicates the placement of the file in the file tree.
    pub fn get_node_id(&self, file_id: &FileId) -> Option<NodeId> {
        match file_id {
            FileId::Inode(inode) => self.node_ids.get(&inode).cloned(),
            FileId::DriveId(drive_id) => self.get_node_id(&FileId::Inode(
                self.get_inode(&FileId::DriveId(drive_id.to_string()))
                    .unwrap(),
            )),
            FileId::NodeId(node_id) => Some(node_id.clone()),
            ref pn @ FileId::ParentAndName { .. } => {
                let inode = self.get_inode(&pn)?;
                self.get_node_id(&FileId::Inode(inode))
            }
        }
    }

    /// Returns the DriveId of a file identified by a given id.
    /// The DriveId points to a Google Drive file.
    pub fn get_drive_id(&self, id: &FileId) -> Option<DriveId> {
        self.get_file(id)?.drive_id()
    }

    /// Returns the inode of a file identified by a given id.
    pub fn get_inode(&self, id: &FileId) -> Option<Inode> {
        match id {
            FileId::Inode(inode) => Some(*inode),
            FileId::DriveId(drive_id) => self.drive_ids.get(drive_id).cloned(),
            FileId::NodeId(node_id) => self
                .tree
                .get(&node_id)
                .map(|node| node.data())
                .ok()
                .cloned(),
            FileId::ParentAndName {
                ref parent,
                ref name,
            } => self
                .get_children(&FileId::Inode(*parent))?
                .into_iter()
                .find(|child| child.name() == *name)
                .map(|child| child.inode()),
        }
    }

    /// Returns the children of a directory identified by a given id.
    pub fn get_children(&self, id: &FileId) -> Option<Vec<&File>> {
        let node_id = self.get_node_id(&id)?;
        let children: Vec<&File> = self
            .tree
            .children(&node_id)
            .unwrap()
            .map(|child| self.get_file(&FileId::Inode(*child.data())))
            .filter(Option::is_some)
            .map(Option::unwrap)
            .collect();

        Some(children)
    }

    /// Returns a const reference to a file identified by a given id.
    pub fn get_file(&self, id: &FileId) -> Option<&File> {
        let inode = self.get_inode(id)?;
        self.files.get(&inode)
    }

    /// Returns a mutable reference to a file identified by a given id.
    pub fn get_mut_file(&mut self, id: &FileId) -> Option<&mut File> {
        let inode = self.get_inode(&id)?;
        self.files.get_mut(&inode)
    }

    /// Creates a file on Drive and adds it to the local file tree.
    pub fn create_file(&mut self, mut file: File, parent: Option<FileId>) -> Result<(), Error> {
        let drive_id = self.df.create(file.drive_file.as_ref().unwrap())?;
        file.set_drive_id(drive_id);
        self.add_file_locally(file, parent)?;

        Ok(())
    }

    /// Passes along the FLUSH system call to the `DriveFacade`.
    pub fn flush(&mut self, id: &FileId) -> Result<(), Error> {
        let file = self
            .get_drive_id(&id)
            .ok_or_else(|| err_msg(format!("Cannot find drive id of {:?}", &id)))?;
        self.df.flush(&file)
    }

    fn get_sibling_count(&self, id: &FileId, parent: &FileId) -> Result<usize, Error> {
        let file = self
            .get_file(id)
            .ok_or_else(|| err_msg(format!("Cannot get_file: {:?}", &id)))?;

        let identical_filename_count = self
            .get_children(&parent)
            .ok_or_else(|| err_msg("FileManager::get_sibling_count() could not get file siblings"))?
            .iter()
            .filter(|child| child.name == file.name)
            .count();

        Ok(identical_filename_count)
    }

    /// Adds a file to the local file tree under its parent. If the parent does not exist, adds the
    /// file as the root node. Does not communicate with Drive.
    fn add_file_locally(&mut self, mut file: File, parent: Option<FileId>) -> Result<(), Error> {
        let node_id = match parent {
            Some(id) => {
                let parent_id = self.get_node_id(&id).ok_or_else(|| {
                    err_msg(format!(
                        "FileManager::add_file_locally() could not find parent: {:?}",
                        id
                    ))
                })?;

                if self.rename_identical_files {
                    let count = self
                        .get_sibling_count(&FileId::Inode(file.inode()), &id)
                        .unwrap_or_default();
                    if count > 1 {
                        file.identical_name_id = Some(count);
                    } else {
                        file.identical_name_id = None;
                    }
                }

                self.tree
                    .insert(Node::new(file.inode()), UnderNode(&parent_id))
            }
            None => self.tree.insert(Node::new(file.inode()), AsRoot),
        }?;

        self.node_ids.insert(file.inode(), node_id);
        file.drive_id()
            .and_then(|drive_id| self.drive_ids.insert(drive_id, file.inode()));
        self.files.insert(file.inode(), file);

        Ok(())
    }

    /// Moves a file somewhere else in the local file tree. Does not communicate with Drive.
    fn move_locally(&mut self, id: &FileId, new_parent: &FileId) -> Result<(), Error> {
        let current_node = self
            .get_node_id(&id)
            .ok_or_else(|| err_msg(format!("Cannot find node_id of {:?}", &id)))?;
        let target_node = self
            .get_node_id(&new_parent)
            .ok_or_else(|| err_msg("Target node doesn't exist"))?;

        self.tree.move_node(&current_node, ToParent(&target_node))?;

        if self.rename_identical_files {
            let count = self.get_sibling_count(id, new_parent)?;
            let mut file = self
                .get_mut_file(id)
                .ok_or_else(|| err_msg(format!("Cannot find file {:?}", &id)))?;

            if count > 1 {
                file.identical_name_id = Some(count);
            } else {
                file.identical_name_id = None;
            }
        }

        Ok(())
    }

    /// Deletes a file and its children from the local file tree. Does not communicate with Drive.
    fn delete_locally(&mut self, id: &FileId) -> Result<(), Error> {
        let node_id = self
            .get_node_id(id)
            .ok_or_else(|| err_msg(format!("Cannot find node_id of {:?}", &id)))?;
        let inode = self
            .get_inode(id)
            .ok_or_else(|| err_msg(format!("Cannot find inode of {:?}", &id)))?;
        let drive_id = self
            .get_drive_id(id)
            .ok_or_else(|| err_msg(format!("Cannot find drive id of {:?}", &id)))?;

        self.tree.remove_node(node_id, DropChildren)?;
        self.files.remove(&inode);
        self.node_ids.remove(&inode);
        self.drive_ids.remove(&drive_id);

        Ok(())
    }

    /// Deletes a file locally *and* on Drive.
    pub fn delete(&mut self, id: &FileId) -> Result<(), Error> {
        let drive_id = self
            .get_drive_id(id)
            .ok_or_else(|| err_msg("No such file"))?;

        self.delete_locally(id)?;
        match self.df.delete_permanently(&drive_id) {
            Ok(response) => {
                debug!("{:?}", response);
                Ok(())
            }
            Err(e) => Err(err_msg(format!("{}", e))),
        }
    }

    /// Moves a file to the Trash directory locally *and* on Drive.
    pub fn move_file_to_trash(&mut self, id: &FileId, also_on_drive: bool) -> Result<(), Error> {
        debug!("Moving {:?} to trash.", &id);
        let node_id = self
            .get_node_id(id)
            .ok_or_else(|| err_msg(format!("Cannot find node_id of {:?}", &id)))?;
        let drive_id = self
            .get_drive_id(id)
            .ok_or_else(|| err_msg(format!("Cannot find drive_id of {:?}", &id)))?;
        let trash_id = self
            .get_node_id(&FileId::Inode(TRASH_INODE))
            .ok_or_else(|| err_msg("Cannot find node_id of Trash dir"))?;

        self.tree.move_node(&node_id, ToParent(&trash_id))?;

        // File cannot be identified by FileId::ParentAndName now because the parent has changed.
        // Using DriveId instead.
        if also_on_drive {
            self.get_mut_file(&FileId::DriveId(drive_id.clone()))
                .ok_or_else(|| err_msg(format!("Cannot find {:?}", &drive_id)))?
                .set_trashed(true)?;
            self.df.move_to_trash(drive_id)?;
        }

        Ok(())
    }

    /// Whether a file is trashed on Drive.
    pub fn file_is_trashed(&mut self, id: &FileId) -> Result<bool, Error> {
        let file = self
            .get_file(id)
            .ok_or_else(|| err_msg(format!("Cannot find node_id of {:?}", &id)))?;

        Ok(file.is_trashed())
    }

    /// Moves/renames a file locally *and* on Drive.
    pub fn rename(
        &mut self,
        id: &FileId,
        new_parent: Inode,
        new_name: String,
    ) -> Result<(), Error> {
        // Identify the file by its inode instead of (parent, name) because both the parent and
        // name will probably change in this method.
        let id = FileId::Inode(
            self.get_inode(id)
                .ok_or_else(|| err_msg(format!("Cannot find node_id of {:?}", &id)))?,
        );

        let current_node = self
            .get_node_id(&id)
            .ok_or_else(|| err_msg(format!("Cannot find node_id of {:?}", &id)))?;
        let target_node = self
            .get_node_id(&FileId::Inode(new_parent))
            .ok_or_else(|| err_msg("Target node doesn't exist"))?;

        self.tree.move_node(&current_node, ToParent(&target_node))?;

        {
            if self.rename_identical_files {
                let count = self.get_sibling_count(&id, &FileId::Inode(new_parent))?;

                let file = self
                    .get_mut_file(&id)
                    .ok_or_else(|| err_msg("File doesn't exist"))?;
                file.name = new_name.clone();

                if count > 0 {
                    file.identical_name_id = Some(count);
                } else {
                    file.identical_name_id = None;
                }
            }
        }

        let drive_id = self
            .get_drive_id(&id)
            .ok_or_else(|| err_msg(format!("Cannot find drive_id of {:?}", &id)))?;
        let parent_id = self
            .get_drive_id(&FileId::Inode(new_parent))
            .ok_or_else(|| {
                err_msg(format!(
                    "Cannot find drive_id of {:?}",
                    &FileId::Inode(new_parent)
                ))
            })?;

        debug!("parent_id: {}", &parent_id);
        self.df.move_to(&drive_id, &parent_id, &new_name)?;
        Ok(())
    }

    /// Writes to a file locally *and* on Drive. Note: the pending write is not necessarily applied
    /// instantly by the `DriveFacade`.
    pub fn write(&mut self, id: FileId, offset: usize, data: &[u8]) {
        let drive_id = self.get_drive_id(&id).unwrap();
        self.df.write(drive_id, offset, data);
    }
}

impl fmt::Debug for FileManager {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        writeln!(f, "FileManager(")?;

        if self.tree.root_node_id().is_none() {
            return writeln!(f, ")");
        }

        let mut stack: Vec<(u32, &NodeId)> = vec![(0, self.tree.root_node_id().unwrap())];

        while !stack.is_empty() {
            let (level, node_id) = stack.pop().unwrap();

            for _ in 0..level {
                write!(f, "\t")?;
            }

            let file = self.get_file(&FileId::NodeId(node_id.clone())).unwrap();
            writeln!(f, "{:3} => {}", file.inode(), file.name)?;

            self.tree.children_ids(node_id).unwrap().for_each(|id| {
                stack.push((level + 1, id));
            });
        }

        writeln!(f, ")")
    }
}
