# zstd:chunked file format

This is a rough documentation of the zstd:chunked file format.  It's a mix of several formats:

 - [tar](https://en.wikipedia.org/wiki/.tar)
 - [zstd](https://github.com/facebook/zstd)
 - [tar-split](https://github.com/vbatts/tar-split/)
 - [CRFS Table of Contents JSON](https://github.com/google/crfs/blob/71d77da419c90be7b05d12e59945ac7a8c94a543/stargz/stargz.go#L108)
 - a "footer"
 - a set of annotations on [OCI Descriptors](https://github.com/opencontainers/image-spec/blob/main/descriptor.md)

The main concept is of a zstd compressed `.tar.zstd` file which, when normally decompressed, produces the desired layer
`.tar`, with the correct checksum. This mode of operations is only intended to be used by clients which are unaware of
`zstd:chunked`, however.

For clients that are aware of the format, `zstd:chunked` takes advantage of the fact that (like tar and gzip) a zstd
file can consist of multiple independent concatenated frames.  It also takes advantage of [zstd Skippable
Frames](https://datatracker.ietf.org/doc/html/rfc8878#name-skippable-frames) to store some metadata at the end of the
file which can be used for finding the frames that we need to download.  Finally, [HTTP Range
Requests](https://www.rfc-editor.org/rfc/rfc7233) are used to only download the frames that we actually require.

This document is written from the standpoint of someone trying to consume the format.

# Overall layout

The `.tar.zstd` file will be compressed as a large number of separate frames, such that each individual non-empty
regular file has its content contained within a single frame.  This typically implies that the tar headers between
regular files will also be separated into their own frames.

At the end of the file are three skippable frames:
 - the compressed "manifest" JSON
 - the compressed "tarsplit" JSON
 - the uncompressed "footer"

In order to do anything useful with the file you first need to locate the manifest and tarsplit JSON files in the
compressed stream.

# Finding the skippable frames

Having the skippable frames at the end of file makes things a bit more difficult: it's generally not possible to find
frames by scanning backwards.  There are two ways to solve this problem:
 - the fixed-sized uncompressed footer
 - OCI descriptor annotations

## The footer

The footer is a fixed-sized uncompressed skip-frame, 64 bytes in content length (72 bytes total length).  A skippable
frame header starts with a 32bit little endian magic number (as per the zstd spec, "any value from 0x184D2A50 to
0x184D2A5F") but we always use `0x184d2a50`.  It's followed by a 32bit little endian size (which is always 64 in our
case).

The content of the footer content consists of the following 64bit little endian integers, in order:
 - manifest start offset, compressed length, uncompressed length
 - manifest type (always 1)
 - tarsplit start offset, compressed length, uncompressed length
 - magic number: 0x78556e496c554e47 (ie: the ascii string "GNUlInUx")

That means that any zstd:chunked file will always end with 72 bytes which look something like:

```
00000000  50 2a 4d 18 40 00 00 00  6c 91 62 06 00 00 00 00  |P*M.@...l.b.....|
00000010  9e 68 0f 00 00 00 00 00  e7 4e 54 00 00 00 00 00  |.h.......NT.....|
00000020  01 00 00 00 00 00 00 00  12 fa 71 06 00 00 00 00  |..........q.....|
00000030  e2 57 09 00 00 00 00 00  76 07 eb 00 00 00 00 00  |.W......v.......|
00000040  47 4e 55 6c 49 6e 55 78                           |GNUlInUx|
```

 - `50 2a 4d 18 40 00 00 00`: skippable frame, size 0x40 (64)
 - `6c 91 62 06 00 00 00 00`: start of the manifest in the compressed stream
 - `9e 68 0f 00 00 00 00 00`: length of the manifest in the compressed stream
 - `e7 4e 54 00 00 00 00 00`: uncompressed size of the manifest
 - `01 00 00 00 00 00 00 00`: manifest type (1)
 - `12 fa 71 06 00 00 00 00`: start of the tarsplit json in the compressed stream
 - `e2 57 09 00 00 00 00 00`: length of the tarsplit json in the compressed stream
 - `76 07 eb 00 00 00 00 00`: uncompressed size of the tarsplit json
 - `47 4e 55 6c 49 6e 55 78`: magic number (`GNUlInUx`)

## The OCI descriptor annotations

Of course, you're probably interested in downloading the layer because it's part of an OCI image.  In that case, the
same information that's contained in the header will have also been encoded as a set of annotations on the descriptor
for the layer:

```json
    {
      "mediaType": "application/vnd.oci.image.layer.v1.tar+zstd",
      "digest": "sha256:20574ef181bd366aa8a344c3e869c95a22429feb00f1c4feeb7fb2fd0e8de71c",
      "size": 108745276,
      "annotations": {
        "io.github.containers.zstd-chunked.manifest-checksum": "sha256:44b5219a19eea4bd8414c2938d0561eebdd053fad8110df7957bee86370ba02b",
        "io.github.containers.zstd-chunked.manifest-position": "107123052:1009822:5525223:1",
        "io.github.containers.zstd-chunked.tarsplit-checksum": "sha256:4041c7b1197a991b90eb5e933c3e96e5f60abc56ed4c0bc926a0d5a2e136ebdc",
        "io.github.containers.zstd-chunked.tarsplit-position": "108132882:612322:15402870"
      }
    }
```

The annotations are:

 - `io.github.containers.zstd-chunked.manifest-checksum`: a digest of the compressed "manifest" JSON
 - `io.github.containers.zstd-chunked.manifest-position`: a `:`-separated 4-tuple containing the manifest location
   information in the same order as it appears in the footer: offset, compressed size, uncompressed size, manifest type
 - `io.github.containers.zstd-chunked.tarsplit-checksum`: a digest of the compressed "tarsplit" JSON
 - `io.github.containers.zstd-chunked.tarsplit-position`: a `:`-separated 3-tuple containing the tarsplit location
   information in the same order as it appears in the footer: offset, compressed size, uncompressed size

# The "manifest" file format

The manifest is obtained by slicing (or fetching) the manifest start offset up to the specified compressed length from
the original file. That range contains a single normal compressed zstd frame which you decompress to get the manifest.
As mentioned above, the manifest is contained inside of a skippable frame, but the offsets for finding the manifest do
not include the skippable frame header, so you don't need to do anything about it.

This file format was originally designed as part of the [Container Registry Filesystem](https://github.com/google/crfs/)
project and contains far more information than is required for incremental downloading.  It more or less duplicates all
of the tar header information in a different format.  It was designed to allow a lazy-downloading filesystem
implementation that could do metadata lookups without having to fetch tar headers that were scattered around the rest of
the file.  We can safely ignore most of it.

At the top-level, it's a JSON dictionary containing two items, and looking something like:

```json
{
  "version": 1,
  "entries": [
    {
      "type": "dir",
      "name": "etc/",
      "mode": 493,
      "modtime": "2025-05-28T02:24:19Z",
      "accesstime": "0001-01-01T00:00:00Z",
      "changetime": "0001-01-01T00:00:00Z",
      "xattrs": {
        "user.overlay.impure": "eQ=="
      }
    },
    {
      "type": "reg",
      "name": "etc/asound.conf",
      "mode": 420,
      "size": 55,
      "modtime": "2025-04-14T00:00:00Z",
      "accesstime": "0001-01-01T00:00:00Z",
      "changetime": "0001-01-01T00:00:00Z",
      "digest": "sha256:3b7fe1f8fd9bb7e1c80fddd31c592bc5b7b7d39e322ded8303c742b2bc1bec31",
      "offset": 210,
      "endOffset": 278
    }
  ]
}
```

The `version` is 1.

`entries` is an array.  Each entry is either a complete description of an entry in the `.tar` file (in which case the
type will be `"reg"`, `"dir"`, `"hardlink"`, `"symlink"`, etc.) or an additional "chunk" (type of `"chunk"`).  Chunk
entries follow the file which is chunked, allowing it to be split into smaller chunks (which may improve incremental
downloads). It's possible to ignore chunks when implementing the file format: even in the presence of chunks, the
information required to download the complete data of the file is available on the main entry: you'll simply get it as
multiple concatenated zstd frames (which, as mentioned above, is still a valid compressed file).

The important fields for knowing what needs to be downloaded are `"digest"`, `"offset"`, and `"endOffset"`.  The digest
provides the main mechanism for knowing if we already have a particular file's contents downloaded and the offsets
provide us with the information we need to fetch it if we don't have it.  The `"digest"` field is a digest of the
uncompressed file data.  The `"offset"` is the offset of the start of the compressed frame and the `"endOffset"` is the
offset past the end of the frame (ie: you need to subtract 1 from this when turning it into a Range request).  `"size"`
is the size of the uncompressed data, and it's useful for verification.

After decompressing, the data from the given range does not contain any extra padding which might be implied by the tar
format (ie: rounding up to the next 512-byte block).  The uncompressed data should match the `"size"` and `"digest"`.

For purposes of incremental downloads, we really only need the entries of `"type": "reg"` with a non-zero `"size"` and
`"digest"` plus `"offset`" and `"endOffset"`.  Those are the entries that will let us find our file content.

## Chunks

The `"type": "chunk"` entries contain information about individual file chunks.  It's not specified which algorithm is
used to determine the chunking, but [containers/storage](https://github.com/containers/storage) uses a rolling checksum
approach.  In an addition to the original format, zstd:chunked can also contain `"chunkType": "zeros"` chunks which are
effectively sparse "holes".

The chunk format is not described here because it's not implemented yet: the most obvious approach to doing so would
require duplicating file content on disk (chunked and merged) and it's not clear if it's worth it to save a bit of extra
downloading.

# The "tarsplit" file format

This is the JSON-lines format produced by the [tar-split](https://github.com/vbatts/tar-split/) utility used in the
implementation of podman, explaining its presence in this file format.

The purpose of this format is to store enough metadata to allow reconstructing the original content of a `.tar` file
which was previously unpacked, assuming we still have the unpacked contents around (referred to by their filenames).

The inclusion of this file essentially reproduces all of the data in the tar headers for a third time.

It looks something like this:

```json
{"type":2,"payload":"ZXRjL1BheEhlYWRlcnMuMAAA...","position":0}
{"type":1,"name":"etc/","payload":null,"position":1}
{"type":2,"payload":"ZXRjL2Fzb3VuZC5jb25mAAA...=","position":2}
{"type":1,"name":"etc/asound.conf","size":55,"payload":"tAt3+IpQDrE=","position":3}
{"type":2,"payload":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA...==","position":4}
```

where each line contains one dictionary which is either `"type": 1` or `"type": 2`.

The "type 1" items are references to file content.  In the originally-conceived use of the tool, these would be files
that you could read from the filesystem to get the content to include back in the reconstructed tar stream.  In our
case, we can find those files in the manifest (along with the information about where we can fetch them and if we need
to).  The important key here is `"name"` which is the filename: it's exactly the same name as in the manifest.  These
entries are really only useful for regular files.  You can identify that case by the presence of a `"size"` field: the
uncompressed file size (which should match the same key in the manifest).  The `"payload"` field on "type 1" entries
contains a base64-encoded crc64 of the file contents.

The "type 2" items are inline content used to encode non-file data.  The tar headers end up reproduced in these.  It's
worth noting that padding is *included* in these items.  That means that the payload of a "type 2" entry following a
"type 1" entry that doesn't have a multiple-of-512 file size will start with padding (seen as "AAAA..." in the above
example).

# Putting it all together

The first step is to get the compressed form of the manifest and the tarsplit JSON files.  You can use the OCI
annotations if you have them, or do a "suffix" range request for the footer.

Once you have the two JSON files you need to decompress them.

Rebuilding the `.tar` file contents then essentially works by iterating over the lines in the tarsplit file.  For each
"type 2" entry, you simply output the inline data.  For each "type 1" entry, you look up the equivalent entry in the
manifest, check if you already have an object with that checksum, use the range information to download it if not, and
then output its content.

In all cases I've seen, the entries in the tarstream and the manifest are in the same order, so it's probably possible
to create an efficient implementation that decompresses them in parallel, keeping them in sync with each other, avoiding
ever having to fully extract either file at the same time.  In practice, the total amount of data in the manifest is
relatively small, and extracting it to some form of a lookup table is probably a more reliable (and easier) approach.

Making individual HTTP requests for each range as we require it is probably not a reasonable idea: there's too much
overhead and latency.  HTTP/2 and HTTP/3 improve things by allowing massive levels of concurrency over a single
connection (saving handshaking overhead) but the overhead of the headers is still significant.  A better approach is to
pre-filter the list of digests, removing the ones we already have, and fetching many ranges per request.
