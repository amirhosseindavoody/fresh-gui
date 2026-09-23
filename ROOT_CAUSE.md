# Source Control changed-file list (#92)

The GUI already requests Git status when a workspace is restored or Source Control is opened. The request carries the active workspace ID, the daemon resolves that workspace's root, and the status result reaches the existing file list. The request path and `CAP_GIT` negotiation were not dropping the files.

On Windows, the daemon's filesystem root is canonicalized to a verbatim path such as `\\?\C:\repo` (or `\\?\UNC\server\share\repo`). That path was passed directly to `git -C`. Git for Windows does not reliably accept the verbatim form as its working directory, so repository discovery failed and the daemon returned `repo: false` with no files. Git commands now receive the equivalent ordinary drive or UNC path; filesystem access continues to use the canonical path.

The porcelain v1 `-z` parser also treated the second record of a rename or copy as the file to display. In this format the first record is the destination, and the second is the source. The parser now keeps the destination and consumes the source, so renamed and copied files remain selectable by their current path.

Regression tests cover the Windows path conversion, porcelain paths and renames, and live status for a non-repository, an empty repository, untracked, staged, and staged-plus-unstaged changes.
