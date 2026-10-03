# fio over NFS — cownfs performance baseline

This documents how to run fio against a cownfs NFS mount for the D2
performance workstream. Run on a machine with the NFS mount (e.g., Heiko's
Mac with /Volumes/cow mounted).

## Prerequisites

- cownfs-server running and mounted via NFSv4.0.
- fio installed (`brew install fio` on Mac, `apt install fio` on Linux).

## Job file: cownfs-fio.job

```ini
[global]
directory=/Volumes/cow/fio-test
direct=1
ioengine=sync
group_reporting=1

[randread-4k]
rw=randread
bs=4k
size=256m
iodepth=1
numjobs=4
runtime=60

[randwrite-4k]
rw=randwrite
bs=4k
size=256m
iodepth=1
numjobs=4
runtime=60

[seqread-1m]
rw=read
bs=1m
size=1g
iodepth=16
numjobs=1
runtime=60

[seqwrite-1m]
rw=write
bs=1m
size=1g
iodepth=16
numjobs=1
runtime=60
```

## Running

```sh
mkdir -p /Volumes/cow/fio-test
fio cownfs-fio.job --output=cownfs-fio-results.json
```

## Recording results

Append the summary to `docs/p7-soak-results.md` with:
- Date and cownfs commit hash.
- fio version and job file.
- IOPS and bandwidth for each job.
- Comparison with previous runs.
