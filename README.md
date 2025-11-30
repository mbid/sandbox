# Sandbox

A docker-based sandbox to let untrusted LLM-enabled agents work on bureaucracy-related problems.
Features:
- Has a command `run` with a name.
  Creates a temporary shallow clone of the current git repo.
  The shallow clone should have the current git repo as remote (sandbox-<name>) and vice-verca.
  Mount the current git repo suitably in read-only mode into the container so that the shallow (or is it called "shared") clone still works.
- Make sure the user ids inside and outside the containers are mapped to each other.
  The user inside should be the same as the user outside.
- Assume the docker image is provided as a Dockerfile in the root directory of the git repo.
  You should build the docker image first if necessary.
  You can determine whether a rebuild is necessary by taking the sha2 hash of the Dockerfile and tagging the image built with that hash.
  Assume the dockerfile accepts suitable args corresponding to the user name, user id and group id.
  Use that to align the internal and external users.
- The working directory and location of the git repo should be the same as externally.
  To accomplish that, you have to point the shared clone to a symlinked that points to the real repo root, at least externally.
  Inside the container, set up mount points such that the symlinked directory path is actuallly the outside repo, while the shared clone git repo is at the same location as externally.
- Use overlay file systems/volumes on various directories to make them accessible inside the container but don't propagate changes outside.
- On launch, creates a temporary shallow clone of the current git repo.
- Don't necessarily tear down shared cloned directories when the container exits.
  Keep them around, but add commands to list existing sandboxed git repos, and delete them.
  If you find docker volumes (needed, I think, for overlay file systems) that belong to sandboxes for which we don't have a directory anymore, tear them down.
- It should be possible to attach a sandbox twice.
  This shouldn't launch the container again, we should attach to the existing one.
- While sandboxes are running, keep the two git repos in sync, in the sense that git fetch and git push are not necessary.
  You probably want to have a wathcer process on the host that listens for file system/dir changes to the .git directories of the clone and main repo.
- Make it work with fish.
  Assume that the Dockerfile installs fish.
  If the ambient user's shell is fish, then also use fish inside the container.
  This reminds me: If no command is provided for the `run` command, drop into an interactive shell.
  Mount the fish config (~/.config/fish) into the container in read-only mode.
- Keep most logic in the rust crate library.
  The binary should have a very simple main fucntion that simply calls into the library.
- Contrary to what I might have written furhter up, the location for sandbox dirs (shared clone, overlay related dirs) should be in $XDG_CACHE_HOME/sandbox/<repo-root-dir-name>-<sha2-of-repo-root-absolute-path>.
  Make sure it works also when $XDG_CACHE_HOME (if that's the var) is not set, defaulting to ~/.cache.
- Make claude work inside the docker container.
  To do that, mount ~/.claude.json and ~/.claude into the container with overlay (no changes propagate outside; writeable with copy on write optimization).
- Block all network traffic by default.
  Specific IPs/domains should be whitelistable though.
  Add the API endpoint of the claude API to the whitelist by default.
  Make the whitelist some constant in the code; doesn't need to be configurable.
