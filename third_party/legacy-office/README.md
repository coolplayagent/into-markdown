# Legacy Office runtime authority

`authority.schema.json` is the packaging contract consumed by the isolated
legacy Office worker. It describes an installed runtime, not a download recipe
that ordinary builds execute. Packaging tasks must generate `authority.json`
from the exact extracted artifact and include every regular file, including all
license and third-party notice files. Empty directories are not authoritative.
`systemLibraries` must be the exact recursive operating-system dependency set
observed in ELF dynamic tags, Mach-O load commands, or PE imports. Every other
dependency must appear in `files` and resolve uniquely through a safe relative
loader path. `systemReadPaths` must cover the declared system identities; it is
not a substitute for inventorying package-owned libraries.

No platform artifact is recorded here until its URL, compressed size and hash,
complete extracted inventory, ABI, and installation-specific license set have
been independently audited. The runtime bundle is not part of the repository or
the current release inventory.

On Windows, the installer owns creation, ACL provisioning, atomic replacement,
and removal of the named AppContainer profile. The converter only consumes the
profile identity after the profile's derived SID exactly matches the authority.
The capability list must remain empty; the explicit forbidden list documents
the network, identity, library, and removable-storage capabilities that the
runtime must never receive.

Issue #23 consumes this schema and enforces the worker/protocol/sandbox boundary.
Platform packaging tasks #133, #134, and #135 remain responsible for generating
the exact artifact inventory, dependency and license authority, immutable install
permissions, and (on Windows) AppContainer profile/ACL transaction.
