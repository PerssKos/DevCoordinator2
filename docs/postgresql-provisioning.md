# PostgreSQL 17 dependency

The reviewed cutover requires PostgreSQL 17 server and client executables in the
permanent, root-owned `/usr/lib/postgresql/17/bin` directory. A worktree's
`target/postgres-fixture-toolchain` is an isolated test dependency, and must not
become the installed service's executable location. Preflight checks all six
executables and their major versions before closing admission or stopping a
service; a missing dependency leaves the existing installation running.

On Debian 13, provision the versioned `postgresql-17` and
`postgresql-client-17` packages through the configured, authenticated Debian
package sources. PostgreSQL documents the
[Debian package installation options](https://www.postgresql.org/download/linux/debian/).

Before installing a server package, inspect existing PostgreSQL clusters and
preserve any existing cluster-manager configuration. Set
`create_main_cluster = false` in `/etc/postgresql-common/createcluster.conf`
before package installation, retaining the existing configuration and this
explicit setting when resolving package configuration prompts. Debian otherwise
creates a `main` cluster automatically; the setting is documented in
[pg_createcluster](https://manpages.debian.org/trixie/postgresql-common/pg_createcluster.1.en.html).
Do not run `pg_createcluster`, start the umbrella `postgresql.service`, or
restart another cluster as part of this dependency step. Confirm that package
installation created no additional cluster or listener and preserve the package
versions and executable hashes in the installation evidence.

Provisioning binaries does not activate the new authority. After candidate
acceptance, the cutover alone creates the owned `devcoordinator2-pg` identity,
initializes its marker-bound data directory, and starts
`devcoordinator2-postgres.service` with a private Unix socket and TCP disabled.
No live provisioning or service change is part of source validation.
