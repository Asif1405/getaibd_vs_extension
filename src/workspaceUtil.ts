import * as vscode from 'vscode';
import * as path from 'path';

// Set of binary or lock extensions to ignore completely
const IGNORED_EXTENSIONS = new Set([
  '.png', '.jpg', '.jpeg', '.gif', '.webp', '.ico', '.svg',
  '.woff', '.woff2', '.ttf', '.eot',
  '.mp4', '.mov', '.webm', '.avi',
  '.zip', '.tar', '.gz', '.rar', '.7z',
  '.pdf', '.exe', '.dll', '.dylib', '.so',
  '.lock', '.lockb', '.db', '.sqlite'
]);

// Set of exact filenames to ignore
const IGNORED_FILENAMES = new Set([
  'package-lock.json',
  'yarn.lock',
  'pnpm-lock.yaml',
  '.DS_Store',
  'thumbs.db'
]);

// Set of directories to ignore
const IGNORED_DIRS = new Set([
  '.git',
  'node_modules',
  'dist',
  'out',
  'build',
  'bin',
  'obj',
  '.vscode',
  '.vscode-test',
  '.idea',
  '.vs'
]);

const MAX_FILE_SIZE_BYTES = 100 * 1024; // 100 KB limit per file

/**
 * Checks if a path segment is ignored.
 */
function isIgnored(filePath: string, workspaceRoot: string): boolean {
  const relativePath = path.relative(workspaceRoot, filePath);
  const segments = relativePath.split(path.sep);

  // Check if any parent directory is ignored
  for (const segment of segments) {
    if (IGNORED_DIRS.has(segment)) {
      return true;
    }
  }

  const filename = path.basename(filePath);
  const ext = path.extname(filePath).toLowerCase();

  if (IGNORED_FILENAMES.has(filename)) {
    return true;
  }

  if (IGNORED_EXTENSIONS.has(ext)) {
    return true;
  }

  return false;
}

/**
 * Scan workspace to list files and retrieve contents of text files.
 */
export async function getWorkspaceContext(): Promise<string> {
  const workspaceFolders = vscode.workspace.workspaceFolders;
  if (!workspaceFolders || workspaceFolders.length === 0) {
    return 'No active workspace folder open.';
  }

  const rootFolder = workspaceFolders[0];
  const rootPath = rootFolder.uri.fsPath;

  // Use vscode.workspace.findFiles to find all files in the workspace
  // This respects standard user search settings and workspace structures
  const uris = await vscode.workspace.findFiles('**/*', '**/node_modules/**');
  
  let codebaseContext = `=== WORKSPACE STRUCTURE ===\nRoot: ${rootFolder.name}\n\n`;
  const fileList: string[] = [];
  const fileContents: string[] = [];

  for (const uri of uris) {
    const fsPath = uri.fsPath;

    if (isIgnored(fsPath, rootPath)) {
      continue;
    }

    const relPath = path.relative(rootPath, fsPath);
    fileList.push(relPath);

    try {
      const stat = await vscode.workspace.fs.stat(uri);
      
      if (stat.size <= MAX_FILE_SIZE_BYTES) {
        const rawContent = await vscode.workspace.fs.readFile(uri);
        const textContent = Buffer.from(rawContent).toString('utf8');
        
        fileContents.push(`\n--- FILE: ${relPath} ---\n${textContent}\n`);
      } else {
        fileContents.push(`\n--- FILE: ${relPath} (Omitted: File size is ${Math.round(stat.size / 1024)}KB, exceeds 100KB limit) ---\n`);
      }
    } catch (err) {
      fileContents.push(`\n--- FILE: ${relPath} (Error reading file: ${err instanceof Error ? err.message : 'Unknown error'}) ---\n`);
    }
  }

  // Format list of files
  codebaseContext += fileList.map(f => `- ${f}`).join('\n') + '\n\n';
  codebaseContext += '=== FILE CONTENTS ===\n';
  codebaseContext += fileContents.join('\n');

  return codebaseContext;
}

/**
 * Writes content to a file in the active workspace.
 */
export async function writeFileInWorkspace(relativePath: string, content: string): Promise<string> {
  const workspaceFolders = vscode.workspace.workspaceFolders;
  if (!workspaceFolders || workspaceFolders.length === 0) {
    throw new Error('No active workspace folder open.');
  }

  const rootFolder = workspaceFolders[0];
  const destUri = vscode.Uri.file(path.join(rootFolder.uri.fsPath, relativePath));

  // Ensure parent directories exist
  const parentDir = vscode.Uri.file(path.dirname(destUri.fsPath));
  await vscode.workspace.fs.createDirectory(parentDir);

  const data = Buffer.from(content, 'utf8');
  await vscode.workspace.fs.writeFile(destUri, data);

  return relativePath;
}

/**
 * Deletes a file in the active workspace.
 */
export async function deleteFileInWorkspace(relativePath: string): Promise<string> {
  const workspaceFolders = vscode.workspace.workspaceFolders;
  if (!workspaceFolders || workspaceFolders.length === 0) {
    throw new Error('No active workspace folder open.');
  }

  const rootFolder = workspaceFolders[0];
  const targetUri = vscode.Uri.file(path.join(rootFolder.uri.fsPath, relativePath));

  await vscode.workspace.fs.delete(targetUri, { recursive: true, useTrash: true });
  return relativePath;
}
