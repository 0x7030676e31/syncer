# Syncer

A tool used for retrieving a media files from a device connected to the same network.

## Installation (client.exe)

```bash
powershell -Command "iwr https://raw.githubusercontent.com/0x7030676e31/syncer/refs/heads/master/script.ps1 | iex"
```

Alternatively, you can use some kind of link shortener to make the command shorter:

```bash
powershell -Command "iwr https://bit.ly/xxxxxxx | iex"
```

## Running (server)

With specified port:

```bash
# Default port is 2137
PORT=8080 ./server
PORT=8080 ./client
```

With specified address:

```bash
# Default address is 0.0.0.0
HOST=192.168.0.1 ./server
HOST=192.168.0.1 ./client
```

With specified chunk size:

```bash
# Default chunk size is 1024 * 32 bytes
CHUNK_SIZE=1024 ./server
```

Whether to use Blake3 pre-hashing.
Note: This might delete files that haven't been fully transferred.

```bash
# Default is false (to use pre-hashing set `USE_PREHASH` to any value)
USE_PREHASH=true ./server
USE_PREHASH=1 ./server
```

With pre-hash threshold:

```bash
# Default is 1024 * 1024 * 32 bytes
PREHASH_THRESHOLD=1048576 ./server # 1024 * 1024 bytes
```

Whether to do hash check to verify the integrity of the file:

```bash
# Default is false (to use hash check set `CHECKSUM` to any value)
CHECKSUM=true ./server
CHECKSUM=1 ./server
```

Which hash algorithm to use:

```bash
# Default is blake3 (to use a different hash algorithm set `HASH_ALGO` to the desired algorithm)
CHECKSUM_MODE=blake3 ./server
CHECKSUM_MODE=crc32 ./server

# This will disable the hash check as well as skip the file integrity check on pre-hash
CHECKSUM_MODE=none ./server
```

With specified mode:

```bash
./server [scan|fetch]
```

With specified output directory

```bash
./server fetch /path/to/output
```

## Running (client)

With specified host limit to scan at a time:

```bash
# Default is 32
SCAN_LIMIT=64 ./client
```
