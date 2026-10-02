//! Protocol constant regression tests: error codes must match RFC 7530
//! (verified against pynfs's generated constants).

use cownfs_nfs::nfs4::*;

#[test]
fn error_codes_match_rfc() {
    // Spot-check the codes that were wrong before.
    assert_eq!(NFS4ERR_NOFILEHANDLE, 10020);
    assert_eq!(NFS4ERR_BAD_SEQID, 10026);
    assert_eq!(NFS4ERR_OPENMODE, 10038);
    // And the neighbors, to catch copy-paste slips.
    assert_eq!(NFS4ERR_NOTDIR, 20);
    assert_eq!(NFS4ERR_ISDIR, 21);
    assert_eq!(NFS4ERR_INVAL, 22);
    assert_eq!(NFS4ERR_BAD_STATEID, 10025);
    // v4.1 session codes (RFC 5661).
    assert_eq!(NFS4ERR_BADSESSION, 10064);
    assert_eq!(NFS4ERR_BADSLOT, 10065);
    assert_eq!(NFS4ERR_SEQ_MISORDERED, 10066);
}

#[test]
fn op_codes_match_rfc() {
    assert_eq!(OP_EXCHANGE_ID, 42);
    assert_eq!(OP_CREATE_SESSION, 43);
    assert_eq!(OP_SEQUENCE, 44);
    assert_eq!(OP_DESTROY_SESSION, 47);
    assert_eq!(OP_LAYOUTGET, 50);
    assert_eq!(OP_LAYOUTCOMMIT, 51);
    assert_eq!(OP_LAYOUTRETURN, 52);
    assert_eq!(OP_DESTROY_CLIENTID, 57);
}
