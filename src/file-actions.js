export function excludeTrashedEntries(entries, removedPaths, platform) {
  const normalize = platform === 'windows'
    ? (path) => path.replaceAll('/', '\\').toLowerCase()
    : (path) => path;
  const separator = platform === 'windows' ? '\\' : '/';
  const roots = [...removedPaths].map((path) => normalize(path).replace(/[\\/]$/, ''));
  if (roots.length === 0) return entries;
  return entries.filter((entry) => {
    const path = normalize(entry.path);
    return !roots.some((root) => path === root || path.startsWith(`${root}${separator}`));
  });
}
