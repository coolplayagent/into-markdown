# Legacy Office runtime authority

`authority.schema.json` is the packaging contract consumed by the isolated
legacy Office worker. It describes an installed runtime, not a download recipe
that ordinary builds execute. Packaging tasks must generate `authority.json`
from the exact extracted artifact and include every regular file, including all
license and third-party notice files. Empty directories are not authoritative.

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
