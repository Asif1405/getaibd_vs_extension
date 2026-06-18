use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::AppError;

use super::call_graph::{CallGraph, CallGraphBuilder, TypeInfo};
use super::project_graph::{ProjectGraph, ProjectGraphBuilder};
use super::recent_tracker::RecentChangesTracker;

#[derive(Clone)]
pub struct ProjectAnalysisCache {
    inner: Arc<RwLock<CacheInner>>,
}

struct CacheInner {
    project_graph: Option<CachedItem<ProjectGraph>>,
    call_graph: Option<CachedItem<CallGraph>>,
    types: Option<CachedItem<HashMap<String, TypeInfo>>>,
    recent_tracker: Option<CachedItem<RecentChangesTracker>>,
    file_mtimes: HashMap<PathBuf, u64>,
    cache_ttl_secs: u64,
}

struct CachedItem<T> {
    data: T,
    timestamp: u64,
}

impl<T> CachedItem<T> {
    fn new(data: T) -> Self {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        Self { data, timestamp }
    }

    fn is_stale(&self, ttl_secs: u64) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        now - self.timestamp > ttl_secs
    }
}

impl ProjectAnalysisCache {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(CacheInner {
                project_graph: None,
                call_graph: None,
                types: None,
                recent_tracker: None,
                file_mtimes: HashMap::new(),
                cache_ttl_secs: 300,
            })),
        }
    }

    pub fn with_ttl(self, ttl_secs: u64) -> Self {
        if let Ok(mut inner) = self.inner.write() {
            inner.cache_ttl_secs = ttl_secs;
        }
        self
    }

    pub fn get_project_graph(&self, project_root: &Path) -> Result<Option<ProjectGraph>, AppError> {
        let inner = self
            .inner
            .read()
            .map_err(|e| AppError::InvalidRequest(format!("Cache lock error: {}", e)))?;

        if let Some(cached) = &inner.project_graph {
            if !cached.is_stale(inner.cache_ttl_secs)
                && !self.has_files_changed(project_root, &inner)?
            {
                return Ok(Some(cached.data.clone()));
            }
        }

        Ok(None)
    }

    pub fn set_project_graph(
        &self,
        graph: ProjectGraph,
        project_root: &Path,
    ) -> Result<(), AppError> {
        let mut inner = self
            .inner
            .write()
            .map_err(|e| AppError::InvalidRequest(format!("Cache lock error: {}", e)))?;

        inner.project_graph = Some(CachedItem::new(graph));
        self.update_file_mtimes(project_root, &mut inner)?;

        Ok(())
    }

    pub fn get_call_graph(&self, project_root: &Path) -> Result<Option<CallGraph>, AppError> {
        let inner = self
            .inner
            .read()
            .map_err(|e| AppError::InvalidRequest(format!("Cache lock error: {}", e)))?;

        if let Some(cached) = &inner.call_graph {
            if !cached.is_stale(inner.cache_ttl_secs)
                && !self.has_files_changed(project_root, &inner)?
            {
                return Ok(Some(cached.data.clone()));
            }
        }

        Ok(None)
    }

    pub fn set_call_graph(&self, graph: CallGraph, project_root: &Path) -> Result<(), AppError> {
        let mut inner = self
            .inner
            .write()
            .map_err(|e| AppError::InvalidRequest(format!("Cache lock error: {}", e)))?;

        inner.call_graph = Some(CachedItem::new(graph));
        self.update_file_mtimes(project_root, &mut inner)?;

        Ok(())
    }

    pub fn get_types(
        &self,
        project_root: &Path,
    ) -> Result<Option<HashMap<String, TypeInfo>>, AppError> {
        let inner = self
            .inner
            .read()
            .map_err(|e| AppError::InvalidRequest(format!("Cache lock error: {}", e)))?;

        if let Some(cached) = &inner.types {
            if !cached.is_stale(inner.cache_ttl_secs)
                && !self.has_files_changed(project_root, &inner)?
            {
                return Ok(Some(cached.data.clone()));
            }
        }

        Ok(None)
    }

    pub fn set_types(
        &self,
        types: HashMap<String, TypeInfo>,
        project_root: &Path,
    ) -> Result<(), AppError> {
        let mut inner = self
            .inner
            .write()
            .map_err(|e| AppError::InvalidRequest(format!("Cache lock error: {}", e)))?;

        inner.types = Some(CachedItem::new(types));
        self.update_file_mtimes(project_root, &mut inner)?;

        Ok(())
    }

    pub fn get_recent_tracker(&self) -> Result<Option<RecentChangesTracker>, AppError> {
        let inner = self
            .inner
            .read()
            .map_err(|e| AppError::InvalidRequest(format!("Cache lock error: {}", e)))?;

        if let Some(cached) = &inner.recent_tracker {
            if !cached.is_stale(inner.cache_ttl_secs) {
                return Ok(Some(cached.data.clone()));
            }
        }

        Ok(None)
    }

    pub fn set_recent_tracker(&self, tracker: RecentChangesTracker) -> Result<(), AppError> {
        let mut inner = self
            .inner
            .write()
            .map_err(|e| AppError::InvalidRequest(format!("Cache lock error: {}", e)))?;

        inner.recent_tracker = Some(CachedItem::new(tracker));

        Ok(())
    }

    pub fn invalidate(&self) -> Result<(), AppError> {
        let mut inner = self
            .inner
            .write()
            .map_err(|e| AppError::InvalidRequest(format!("Cache lock error: {}", e)))?;

        inner.project_graph = None;
        inner.call_graph = None;
        inner.types = None;
        inner.recent_tracker = None;
        inner.file_mtimes.clear();

        Ok(())
    }

    pub fn invalidate_if_changed(&self, project_root: &Path) -> Result<bool, AppError> {
        let inner = self
            .inner
            .read()
            .map_err(|e| AppError::InvalidRequest(format!("Cache lock error: {}", e)))?;

        if self.has_files_changed(project_root, &inner)? {
            drop(inner);
            self.invalidate()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn has_files_changed(
        &self,
        _project_root: &Path,
        inner: &CacheInner,
    ) -> Result<bool, AppError> {
        if inner.file_mtimes.is_empty() {
            return Ok(true);
        }

        for (path, cached_mtime) in &inner.file_mtimes {
            if let Ok(metadata) = fs::metadata(path) {
                if let Ok(modified) = metadata.modified() {
                    let current_mtime = modified.duration_since(UNIX_EPOCH).unwrap().as_secs();

                    if current_mtime > *cached_mtime {
                        return Ok(true);
                    }
                }
            }
        }

        Ok(false)
    }

    fn update_file_mtimes(
        &self,
        project_root: &Path,
        inner: &mut CacheInner,
    ) -> Result<(), AppError> {
        inner.file_mtimes.clear();

        let src_dir = project_root.join("src");
        if !src_dir.exists() {
            return Ok(());
        }

        self.walk_and_record_mtimes(&src_dir, inner)?;

        Ok(())
    }

    fn walk_and_record_mtimes(&self, dir: &Path, inner: &mut CacheInner) -> Result<(), AppError> {
        for entry in fs::read_dir(dir)
            .map_err(|e| AppError::InvalidRequest(format!("Failed to read directory: {}", e)))?
        {
            let entry = entry
                .map_err(|e| AppError::InvalidRequest(format!("Failed to read entry: {}", e)))?;

            let path = entry.path();

            if path.is_dir() {
                self.walk_and_record_mtimes(&path, inner)?;
            } else if path.extension().and_then(|s| s.to_str()) == Some("rs") {
                if let Ok(metadata) = fs::metadata(&path) {
                    if let Ok(modified) = metadata.modified() {
                        let mtime = modified.duration_since(UNIX_EPOCH).unwrap().as_secs();
                        inner.file_mtimes.insert(path, mtime);
                    }
                }
            }
        }

        Ok(())
    }
}

pub async fn get_or_build_project_graph(
    cache: &ProjectAnalysisCache,
    project_root: &Path,
) -> Result<ProjectGraph, AppError> {
    if let Some(graph) = cache.get_project_graph(project_root)? {
        return Ok(graph);
    }

    let graph = ProjectGraphBuilder::new(project_root.to_path_buf()).build()?;
    cache.set_project_graph(graph.clone(), project_root)?;

    Ok(graph)
}

pub async fn get_or_build_call_graph(
    cache: &ProjectAnalysisCache,
    project_root: &Path,
) -> Result<(CallGraph, HashMap<String, TypeInfo>), AppError> {
    if let (Some(graph), Some(types)) = (
        cache.get_call_graph(project_root)?,
        cache.get_types(project_root)?,
    ) {
        return Ok((graph, types));
    }

    let mut builder = CallGraphBuilder::new();
    let src_dir = project_root.join("src");

    if src_dir.exists() {
        walk_and_analyze(&src_dir, &mut builder)?;
    }

    let (graph, types) = builder.build();
    cache.set_call_graph(graph.clone(), project_root)?;
    cache.set_types(types.clone(), project_root)?;

    Ok((graph, types))
}

fn walk_and_analyze(dir: &Path, builder: &mut CallGraphBuilder) -> Result<(), AppError> {
    for entry in fs::read_dir(dir)
        .map_err(|e| AppError::InvalidRequest(format!("Failed to read directory: {}", e)))?
    {
        let entry =
            entry.map_err(|e| AppError::InvalidRequest(format!("Failed to read entry: {}", e)))?;

        let path = entry.path();

        if path.is_dir() {
            walk_and_analyze(&path, builder)?;
        } else if path.extension().and_then(|s| s.to_str()) == Some("rs") {
            let _ = builder.analyze_file(&path);
        }
    }

    Ok(())
}

pub async fn get_or_build_recent_tracker(
    cache: &ProjectAnalysisCache,
    project_root: &Path,
) -> Result<RecentChangesTracker, AppError> {
    if let Some(tracker) = cache.get_recent_tracker()? {
        return Ok(tracker);
    }

    let tracker = RecentChangesTracker::new(project_root)?;
    cache.set_recent_tracker(tracker.clone())?;

    Ok(tracker)
}
