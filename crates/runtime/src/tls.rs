//! TLS for connections, from the operating system: SChannel on Windows,
//! OpenSSL on Linux and the other Unix systems that have it, loaded when
//! first needed, and in Android apps the platform's `SSLEngine`. The
//! system's certificate store decides which servers are trusted.
//!
//! A session never touches the socket. The connection gives it what has
//! arrived, and sends what it puts out, so that the handshake and every
//! read and write can wait for the socket like the rest of a connection's
//! operations, and run again where they left off.

use crate::host::HostError;

/// What a step of the handshake reached. A system without TLS, such as
/// WASI, never starts a session, so never reaches either.
#[cfg_attr(not(any(windows, unix)), allow(dead_code))]
pub(crate) enum Step {
    Done,
    /// The other side's next messages are needed.
    NeedMore,
}

/// What decrypting what has arrived found.
pub(crate) enum Read {
    /// Records decrypted so far; the rest of a record is still to come.
    More,
    /// The other side ended the session.
    Closed,
}

/// The TLS session of a connection, as a client.
pub(crate) struct Session {
    inner: platform::Session,
    /// What has arrived and is not decrypted yet.
    pub(crate) incoming: Vec<u8>,
    established: bool,
    /// The other side started a handshake again, which goes on before
    /// anything more can be read.
    handshaking_again: bool,
}

impl Session {
    /// A session with the server named `server`, whose certificate must be
    /// for that name.
    pub(crate) fn client(server: &str) -> Result<Session, HostError> {
        Ok(Session {
            inner: platform::Session::client(server)?,
            incoming: Vec::new(),
            established: false,
            handshaking_again: false,
        })
    }

    pub(crate) fn established(&self) -> bool {
        self.established
    }

    /// Goes on with the handshake as far as what has arrived allows,
    /// putting the messages to send in `output`.
    pub(crate) fn handshake(&mut self, output: &mut Vec<u8>) -> Result<Step, HostError> {
        let step = self.inner.handshake(&mut self.incoming, output)?;
        if let Step::Done = step {
            self.established = true;
        }
        Ok(step)
    }

    /// Decrypts the whole records that have arrived into `plain`.
    pub(crate) fn decrypt(
        &mut self,
        plain: &mut Vec<u8>,
        output: &mut Vec<u8>,
    ) -> Result<Read, HostError> {
        loop {
            if self.handshaking_again {
                match self.inner.handshake(&mut self.incoming, output)? {
                    Step::Done => self.handshaking_again = false,
                    Step::NeedMore => return Ok(Read::More),
                }
            }
            match self.inner.decrypt(&mut self.incoming, plain, output)? {
                platform::Decrypted::More => return Ok(Read::More),
                platform::Decrypted::Closed => return Ok(Read::Closed),
                platform::Decrypted::HandshakeAgain => self.handshaking_again = true,
            }
        }
    }

    /// Encrypts `bytes` into records in `output`.
    pub(crate) fn encrypt(&mut self, bytes: &[u8], output: &mut Vec<u8>) -> Result<(), HostError> {
        self.inner.encrypt(bytes, output)
    }

    /// Puts the message that ends the session in `output`.
    pub(crate) fn close(&mut self, output: &mut Vec<u8>) {
        if self.established {
            self.inner.close(output);
            self.established = false;
        }
    }
}

#[cfg(windows)]
mod platform {
    use super::Step;
    use crate::host::HostError;
    use std::ffi::c_void;
    use std::ptr::{null, null_mut};
    use std::sync::OnceLock;

    /// A credential or context handle.
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct Handle {
        lower: usize,
        upper: usize,
    }

    #[repr(C)]
    struct Buffer {
        size: u32,
        kind: u32,
        data: *mut c_void,
    }

    #[repr(C)]
    struct Buffers {
        version: u32,
        count: u32,
        buffers: *mut Buffer,
    }

    #[repr(C)]
    #[derive(Default)]
    struct Sizes {
        header: u32,
        trailer: u32,
        maximum: u32,
        buffers: u32,
        block: u32,
    }

    /// `SCHANNEL_CRED`.
    #[repr(C)]
    struct Credentials {
        version: u32,
        certificates: u32,
        certificate: *mut *const c_void,
        root_store: *mut c_void,
        mappers: u32,
        mapper: *mut *mut c_void,
        algorithms: u32,
        algorithm: *mut u32,
        protocols: u32,
        minimum_strength: u32,
        maximum_strength: u32,
        lifespan: u32,
        flags: u32,
        format: u32,
    }

    const SCHANNEL_CRED_VERSION: u32 = 4;
    const SCH_CRED_NO_DEFAULT_CREDS: u32 = 0x10;
    const SCH_CRED_AUTO_CRED_VALIDATION: u32 = 0x20;
    const SCH_USE_STRONG_CRYPTO: u32 = 0x0040_0000;
    const SECPKG_CRED_OUTBOUND: u32 = 2;
    const SECPKG_ATTR_STREAM_SIZES: u32 = 4;
    const SCHANNEL_SHUTDOWN: u32 = 1;

    const ISC_REQ_REPLAY_DETECT: u32 = 0x4;
    const ISC_REQ_SEQUENCE_DETECT: u32 = 0x8;
    const ISC_REQ_CONFIDENTIALITY: u32 = 0x10;
    const ISC_REQ_USE_SUPPLIED_CREDS: u32 = 0x80;
    const ISC_REQ_ALLOCATE_MEMORY: u32 = 0x100;
    const ISC_REQ_EXTENDED_ERROR: u32 = 0x4000;
    const ISC_REQ_STREAM: u32 = 0x8000;
    const REQUEST: u32 = ISC_REQ_REPLAY_DETECT
        | ISC_REQ_SEQUENCE_DETECT
        | ISC_REQ_CONFIDENTIALITY
        | ISC_REQ_USE_SUPPLIED_CREDS
        | ISC_REQ_ALLOCATE_MEMORY
        | ISC_REQ_EXTENDED_ERROR
        | ISC_REQ_STREAM;

    const SECBUFFER_EMPTY: u32 = 0;
    const SECBUFFER_DATA: u32 = 1;
    const SECBUFFER_TOKEN: u32 = 2;
    const SECBUFFER_EXTRA: u32 = 5;
    const SECBUFFER_STREAM_TRAILER: u32 = 6;
    const SECBUFFER_STREAM_HEADER: u32 = 7;
    const SECBUFFER_ALERT: u32 = 17;

    const SEC_E_OK: i32 = 0;
    const SEC_I_CONTINUE_NEEDED: i32 = 0x0009_0312;
    const SEC_I_CONTEXT_EXPIRED: i32 = 0x0009_0317;
    const SEC_I_INCOMPLETE_CREDENTIALS: i32 = 0x0009_0320;
    const SEC_I_RENEGOTIATE: i32 = 0x0009_0321;
    const SEC_E_INCOMPLETE_MESSAGE: i32 = 0x8009_0318_u32 as i32;

    type Acquire = unsafe extern "system" fn(
        *const u16,
        *const u16,
        u32,
        *mut c_void,
        *mut c_void,
        *mut c_void,
        *mut c_void,
        *mut Handle,
        *mut i64,
    ) -> i32;
    type Initialize = unsafe extern "system" fn(
        *mut Handle,
        *mut Handle,
        *const u16,
        u32,
        u32,
        u32,
        *mut Buffers,
        u32,
        *mut Handle,
        *mut Buffers,
        *mut u32,
        *mut i64,
    ) -> i32;
    type Release = unsafe extern "system" fn(*mut Handle) -> i32;
    type FreeBuffer = unsafe extern "system" fn(*mut c_void) -> i32;
    type Query = unsafe extern "system" fn(*mut Handle, u32, *mut c_void) -> i32;
    type Encrypt = unsafe extern "system" fn(*mut Handle, u32, *mut Buffers, u32) -> i32;
    type Decrypt = unsafe extern "system" fn(*mut Handle, *mut Buffers, u32, *mut u32) -> i32;
    type Control = unsafe extern "system" fn(*mut Handle, *mut Buffers) -> i32;

    /// The functions of SChannel's interface (SSPI).
    struct Sspi {
        acquire: Acquire,
        initialize: Initialize,
        delete_context: Release,
        free_credentials: Release,
        free_buffer: FreeBuffer,
        query: Query,
        encrypt: Encrypt,
        decrypt: Decrypt,
        control: Control,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LoadLibraryW(name: *const u16) -> *mut c_void;
        fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
    }

    fn sspi() -> Result<&'static Sspi, HostError> {
        static SSPI: OnceLock<Option<Sspi>> = OnceLock::new();
        SSPI.get_or_init(|| {
            let name: Vec<u16> = "secur32.dll".encode_utf16().chain(Some(0)).collect();
            // SAFETY: a NUL-terminated name; each symbol, when found, has
            // the type of SSPI's function of that name.
            unsafe {
                let module = LoadLibraryW(name.as_ptr());
                if module.is_null() {
                    return None;
                }
                let symbol = |name: &str| {
                    let name = format!("{name}\0");
                    let address = GetProcAddress(module, name.as_ptr());
                    (!address.is_null()).then_some(address)
                };
                Some(Sspi {
                    acquire: std::mem::transmute::<*mut c_void, Acquire>(symbol(
                        "AcquireCredentialsHandleW",
                    )?),
                    initialize: std::mem::transmute::<*mut c_void, Initialize>(symbol(
                        "InitializeSecurityContextW",
                    )?),
                    delete_context: std::mem::transmute::<*mut c_void, Release>(symbol(
                        "DeleteSecurityContext",
                    )?),
                    free_credentials: std::mem::transmute::<*mut c_void, Release>(symbol(
                        "FreeCredentialsHandle",
                    )?),
                    free_buffer: std::mem::transmute::<*mut c_void, FreeBuffer>(symbol(
                        "FreeContextBuffer",
                    )?),
                    query: std::mem::transmute::<*mut c_void, Query>(symbol(
                        "QueryContextAttributesW",
                    )?),
                    encrypt: std::mem::transmute::<*mut c_void, Encrypt>(symbol("EncryptMessage")?),
                    decrypt: std::mem::transmute::<*mut c_void, Decrypt>(symbol("DecryptMessage")?),
                    control: std::mem::transmute::<*mut c_void, Control>(symbol(
                        "ApplyControlToken",
                    )?),
                })
            }
        })
        .as_ref()
        .ok_or_else(|| {
            (
                "TlsError",
                "TLS is not available: secur32.dll could not be loaded".to_string(),
            )
        })
    }

    pub(super) enum Decrypted {
        More,
        Closed,
        /// The other side starts a handshake again; its messages are in
        /// what arrived.
        HandshakeAgain,
    }

    pub(super) struct Session {
        credentials: Handle,
        context: Option<Handle>,
        /// The server's name, for its certificate, NUL-terminated.
        target: Vec<u16>,
        name: String,
        sizes: Sizes,
    }

    impl Session {
        pub(super) fn client(server: &str) -> Result<Session, HostError> {
            let sspi = sspi()?;
            let mut credentials = Handle::default();
            let mut settings = Credentials {
                version: SCHANNEL_CRED_VERSION,
                certificates: 0,
                certificate: null_mut(),
                root_store: null_mut(),
                mappers: 0,
                mapper: null_mut(),
                algorithms: 0,
                algorithm: null_mut(),
                protocols: 0,
                minimum_strength: 0,
                maximum_strength: 0,
                lifespan: 0,
                flags: SCH_CRED_NO_DEFAULT_CREDS
                    | SCH_CRED_AUTO_CRED_VALIDATION
                    | SCH_USE_STRONG_CRYPTO,
                format: 0,
            };
            let package: Vec<u16> = "Microsoft Unified Security Protocol Provider"
                .encode_utf16()
                .chain(Some(0))
                .collect();
            // SAFETY: the settings and handle outlive the call, which reads
            // and writes nothing else.
            let status = unsafe {
                (sspi.acquire)(
                    null(),
                    package.as_ptr(),
                    SECPKG_CRED_OUTBOUND,
                    null_mut(),
                    (&mut settings as *mut Credentials).cast(),
                    null_mut(),
                    null_mut(),
                    &mut credentials,
                    null_mut(),
                )
            };
            if status != SEC_E_OK {
                return Err(failure(status, server));
            }
            Ok(Session {
                credentials,
                context: None,
                target: server.encode_utf16().chain(Some(0)).collect(),
                name: server.to_string(),
                sizes: Sizes::default(),
            })
        }

        /// Gives SChannel what has arrived, and takes the messages it has
        /// to send, until the handshake is done or needs more.
        pub(super) fn handshake(
            &mut self,
            incoming: &mut Vec<u8>,
            output: &mut Vec<u8>,
        ) -> Result<Step, HostError> {
            let sspi = sspi()?;
            loop {
                let first = self.context.is_none();
                if !first && incoming.is_empty() {
                    return Ok(Step::NeedMore);
                }
                let mut input = [
                    Buffer {
                        size: incoming.len() as u32,
                        kind: SECBUFFER_TOKEN,
                        data: incoming.as_mut_ptr().cast(),
                    },
                    Buffer {
                        size: 0,
                        kind: SECBUFFER_EMPTY,
                        data: null_mut(),
                    },
                ];
                let mut input_list = Buffers {
                    version: 0,
                    count: 2,
                    buffers: input.as_mut_ptr(),
                };
                let mut out = [
                    Buffer {
                        size: 0,
                        kind: SECBUFFER_TOKEN,
                        data: null_mut(),
                    },
                    Buffer {
                        size: 0,
                        kind: SECBUFFER_ALERT,
                        data: null_mut(),
                    },
                ];
                let mut out_list = Buffers {
                    version: 0,
                    count: 2,
                    buffers: out.as_mut_ptr(),
                };
                let mut context = self.context.unwrap_or_default();
                let mut attributes = 0u32;
                // SAFETY: every buffer and handle outlives the call; SChannel
                // reads `incoming` in place and allocates what it puts out.
                let status = unsafe {
                    (sspi.initialize)(
                        &mut self.credentials,
                        if first { null_mut() } else { &mut context },
                        self.target.as_ptr(),
                        REQUEST,
                        0,
                        0,
                        if first { null_mut() } else { &mut input_list },
                        0,
                        &mut context,
                        &mut out_list,
                        &mut attributes,
                        null_mut(),
                    )
                };
                if !first || status >= 0 {
                    self.context = Some(context);
                }
                take_tokens(sspi, &out, output);
                if status == SEC_E_INCOMPLETE_MESSAGE {
                    return Ok(Step::NeedMore);
                }
                if !first {
                    keep_extra(incoming, &input[1]);
                }
                match status {
                    SEC_E_OK => {
                        // SAFETY: an established context, and room for the
                        // sizes.
                        let status = unsafe {
                            (sspi.query)(
                                self.context.as_mut().expect("a context"),
                                SECPKG_ATTR_STREAM_SIZES,
                                (&mut self.sizes as *mut Sizes).cast(),
                            )
                        };
                        if status != SEC_E_OK {
                            return Err(failure(status, &self.name));
                        }
                        return Ok(Step::Done);
                    }
                    SEC_I_CONTINUE_NEEDED => {}
                    SEC_I_INCOMPLETE_CREDENTIALS => {
                        return Err((
                            "TlsError",
                            format!(
                                "{} asks for a client certificate, which connections do not offer yet",
                                self.name
                            ),
                        ));
                    }
                    status => return Err(failure(status, &self.name)),
                }
            }
        }

        pub(super) fn decrypt(
            &mut self,
            incoming: &mut Vec<u8>,
            plain: &mut Vec<u8>,
            _output: &mut Vec<u8>,
        ) -> Result<Decrypted, HostError> {
            let sspi = sspi()?;
            loop {
                if incoming.is_empty() {
                    return Ok(Decrypted::More);
                }
                let mut buffers = [
                    Buffer {
                        size: incoming.len() as u32,
                        kind: SECBUFFER_DATA,
                        data: incoming.as_mut_ptr().cast(),
                    },
                    empty(),
                    empty(),
                    empty(),
                ];
                let mut list = Buffers {
                    version: 0,
                    count: 4,
                    buffers: buffers.as_mut_ptr(),
                };
                // SAFETY: an established context; SChannel decrypts in place
                // and points the buffers into `incoming`.
                let status = unsafe {
                    (sspi.decrypt)(
                        self.context.as_mut().expect("a context"),
                        &mut list,
                        0,
                        null_mut(),
                    )
                };
                match status {
                    SEC_E_INCOMPLETE_MESSAGE => return Ok(Decrypted::More),
                    SEC_E_OK | SEC_I_RENEGOTIATE | SEC_I_CONTEXT_EXPIRED => {
                        let mut extra = Vec::new();
                        for buffer in &buffers {
                            if buffer.size == 0 || buffer.data.is_null() {
                                continue;
                            }
                            // SAFETY: SChannel points the buffers into
                            // `incoming`, which is still whole.
                            let bytes = unsafe {
                                std::slice::from_raw_parts(
                                    buffer.data as *const u8,
                                    buffer.size as usize,
                                )
                            };
                            match buffer.kind {
                                // Only a record that decrypted holds data:
                                // the one that ends the session is left as
                                // it came.
                                SECBUFFER_DATA if status == SEC_E_OK => {
                                    plain.extend_from_slice(bytes)
                                }
                                SECBUFFER_EXTRA => extra = bytes.to_vec(),
                                _ => {}
                            }
                        }
                        *incoming = extra;
                        match status {
                            SEC_I_CONTEXT_EXPIRED => return Ok(Decrypted::Closed),
                            SEC_I_RENEGOTIATE => return Ok(Decrypted::HandshakeAgain),
                            _ => {}
                        }
                    }
                    status => return Err(failure(status, &self.name)),
                }
            }
        }

        pub(super) fn encrypt(
            &mut self,
            bytes: &[u8],
            output: &mut Vec<u8>,
        ) -> Result<(), HostError> {
            let sspi = sspi()?;
            let header = self.sizes.header as usize;
            let trailer = self.sizes.trailer as usize;
            let largest = (self.sizes.maximum as usize).max(1);
            for chunk in bytes.chunks(largest) {
                let mut message = vec![0u8; header + chunk.len() + trailer];
                message[header..header + chunk.len()].copy_from_slice(chunk);
                let base = message.as_mut_ptr();
                // SAFETY: the three parts lie within `message`.
                let mut buffers = unsafe {
                    [
                        Buffer {
                            size: header as u32,
                            kind: SECBUFFER_STREAM_HEADER,
                            data: base.cast(),
                        },
                        Buffer {
                            size: chunk.len() as u32,
                            kind: SECBUFFER_DATA,
                            data: base.add(header).cast(),
                        },
                        Buffer {
                            size: trailer as u32,
                            kind: SECBUFFER_STREAM_TRAILER,
                            data: base.add(header + chunk.len()).cast(),
                        },
                        empty(),
                    ]
                };
                let mut list = Buffers {
                    version: 0,
                    count: 4,
                    buffers: buffers.as_mut_ptr(),
                };
                // SAFETY: an established context, encrypting `message` in
                // place.
                let status = unsafe {
                    (sspi.encrypt)(self.context.as_mut().expect("a context"), 0, &mut list, 0)
                };
                if status != SEC_E_OK {
                    return Err(failure(status, &self.name));
                }
                let length = (buffers[0].size + buffers[1].size + buffers[2].size) as usize;
                output.extend_from_slice(&message[..length]);
            }
            Ok(())
        }

        /// The message that tells the other side the session ends.
        pub(super) fn close(&mut self, output: &mut Vec<u8>) {
            let Ok(sspi) = sspi() else {
                return;
            };
            let Some(context) = self.context.as_mut() else {
                return;
            };
            let mut kind = SCHANNEL_SHUTDOWN;
            let mut token = [Buffer {
                size: 4,
                kind: SECBUFFER_TOKEN,
                data: (&mut kind as *mut u32).cast(),
            }];
            let mut list = Buffers {
                version: 0,
                count: 1,
                buffers: token.as_mut_ptr(),
            };
            // SAFETY: an established context and a token that outlives the
            // call.
            if unsafe { (sspi.control)(context, &mut list) } != SEC_E_OK {
                return;
            }
            let mut out = [Buffer {
                size: 0,
                kind: SECBUFFER_TOKEN,
                data: null_mut(),
            }];
            let mut out_list = Buffers {
                version: 0,
                count: 1,
                buffers: out.as_mut_ptr(),
            };
            let mut attributes = 0u32;
            let mut next = *context;
            // SAFETY: as in the handshake, with no input.
            unsafe {
                (sspi.initialize)(
                    &mut self.credentials,
                    context,
                    self.target.as_ptr(),
                    REQUEST,
                    0,
                    0,
                    null_mut(),
                    0,
                    &mut next,
                    &mut out_list,
                    &mut attributes,
                    null_mut(),
                );
            }
            take_tokens(sspi, &out, output);
        }
    }

    impl Drop for Session {
        fn drop(&mut self) {
            if let Ok(sspi) = sspi() {
                // SAFETY: handles of this session, released once.
                unsafe {
                    if let Some(context) = self.context.as_mut() {
                        (sspi.delete_context)(context);
                    }
                    (sspi.free_credentials)(&mut self.credentials);
                }
            }
        }
    }

    fn empty() -> Buffer {
        Buffer {
            size: 0,
            kind: SECBUFFER_EMPTY,
            data: null_mut(),
        }
    }

    /// Moves the messages SChannel put out into `output`, and frees them.
    fn take_tokens(sspi: &Sspi, buffers: &[Buffer], output: &mut Vec<u8>) {
        for buffer in buffers {
            if buffer.data.is_null() {
                continue;
            }
            if buffer.size > 0 && buffer.kind == SECBUFFER_TOKEN {
                // SAFETY: a buffer SChannel allocated, of that size.
                let bytes = unsafe {
                    std::slice::from_raw_parts(buffer.data as *const u8, buffer.size as usize)
                };
                output.extend_from_slice(bytes);
            }
            // SAFETY: allocated by SChannel for this call.
            unsafe { (sspi.free_buffer)(buffer.data) };
        }
    }

    /// Keeps what SChannel left unread of what arrived: the start of the
    /// next messages.
    fn keep_extra(incoming: &mut Vec<u8>, extra: &Buffer) {
        if extra.kind == SECBUFFER_EXTRA && extra.size > 0 {
            let start = incoming.len() - extra.size as usize;
            incoming.drain(..start);
        } else {
            incoming.clear();
        }
    }

    fn failure(status: i32, name: &str) -> HostError {
        let message = match status as u32 {
            0x8009_0325 => {
                format!("the certificate of {name} is not from an authority this computer trusts")
            }
            0x8009_0322 => format!("the certificate {name} gave is for another name"),
            0x8009_0328 => format!("the certificate of {name} has expired, or is not valid yet"),
            0x8009_2010 => format!("the certificate of {name} was revoked"),
            0x8009_0327 => format!("the certificate of {name} could not be checked"),
            0x8009_0326 | 0x8009_0308 => format!("{name} did not answer with TLS"),
            0x8009_0330 => format!("what {name} sent could not be decrypted"),
            0x8009_0331 => format!("{name} and this computer have no version of TLS in common"),
            other => format!("TLS with {name} failed (error 0x{other:08X})"),
        };
        ("TlsError", message)
    }
}

#[cfg(all(unix, not(target_os = "android")))]
mod platform {
    use super::Step;
    use crate::host::HostError;
    use std::ffi::{CStr, CString, c_char, c_int, c_long, c_void};
    use std::sync::OnceLock;

    type Pointer = *mut c_void;

    /// The functions of OpenSSL's interface, from libssl and the libcrypto
    /// it brings.
    struct OpenSsl {
        method: unsafe extern "C" fn() -> Pointer,
        context_new: unsafe extern "C" fn(Pointer) -> Pointer,
        context_free: unsafe extern "C" fn(Pointer),
        default_paths: unsafe extern "C" fn(Pointer) -> c_int,
        set_verify: unsafe extern "C" fn(Pointer, c_int, Pointer),
        new: unsafe extern "C" fn(Pointer) -> Pointer,
        free: unsafe extern "C" fn(Pointer),
        set_host: unsafe extern "C" fn(Pointer, *const c_char) -> c_int,
        control: unsafe extern "C" fn(Pointer, c_int, c_long, Pointer) -> c_long,
        parameters: unsafe extern "C" fn(Pointer) -> Pointer,
        set_ip: unsafe extern "C" fn(Pointer, *const c_char) -> c_int,
        set_bio: unsafe extern "C" fn(Pointer, Pointer, Pointer),
        connect_state: unsafe extern "C" fn(Pointer),
        handshake: unsafe extern "C" fn(Pointer) -> c_int,
        error: unsafe extern "C" fn(Pointer, c_int) -> c_int,
        read: unsafe extern "C" fn(Pointer, *mut c_void, c_int) -> c_int,
        write: unsafe extern "C" fn(Pointer, *const c_void, c_int) -> c_int,
        shutdown: unsafe extern "C" fn(Pointer) -> c_int,
        verify_result: unsafe extern "C" fn(Pointer) -> c_long,
        bio_memory: unsafe extern "C" fn() -> Pointer,
        bio_new: unsafe extern "C" fn(Pointer) -> Pointer,
        bio_read: unsafe extern "C" fn(Pointer, *mut c_void, c_int) -> c_int,
        bio_write: unsafe extern "C" fn(Pointer, *const c_void, c_int) -> c_int,
        next_error: unsafe extern "C" fn() -> std::ffi::c_ulong,
        error_text: unsafe extern "C" fn(std::ffi::c_ulong, *mut c_char, usize),
        clear_errors: unsafe extern "C" fn(),
    }

    // SAFETY: the table holds addresses of functions, which any thread may
    // call.
    unsafe impl Send for OpenSsl {}
    unsafe impl Sync for OpenSsl {}

    const RTLD_NOW: c_int = 2;
    const SSL_VERIFY_PEER: c_int = 1;
    const SSL_CTRL_SET_TLSEXT_HOSTNAME: c_int = 55;
    const TLSEXT_NAMETYPE_HOST_NAME: c_long = 0;
    const SSL_ERROR_WANT_READ: c_int = 2;
    const SSL_ERROR_WANT_WRITE: c_int = 3;
    const SSL_ERROR_ZERO_RETURN: c_int = 6;

    unsafe extern "C" {
        fn dlopen(file: *const c_char, mode: c_int) -> Pointer;
        fn dlsym(handle: Pointer, name: *const c_char) -> Pointer;
    }

    // Each symbol becomes the function type of the field it fills, which
    // the table's declaration names.
    #[allow(clippy::missing_transmute_annotations)]
    fn openssl() -> Result<&'static OpenSsl, HostError> {
        static OPENSSL: OnceLock<Option<OpenSsl>> = OnceLock::new();
        OPENSSL
            .get_or_init(|| {
                // SAFETY: NUL-terminated names; each symbol, when found, has
                // the type of OpenSSL's function of that name.
                unsafe {
                    let library = [
                        "libssl.so.3",
                        "libssl.so.1.1",
                        "libssl.3.dylib",
                        "libssl.so",
                    ]
                    .iter()
                    .find_map(|name| {
                        let name = CString::new(*name).ok()?;
                        let handle = dlopen(name.as_ptr(), RTLD_NOW);
                        (!handle.is_null()).then_some(handle)
                    })?;
                    let symbol = |name: &str| {
                        let name = CString::new(name).ok()?;
                        let address = dlsym(library, name.as_ptr());
                        (!address.is_null()).then_some(address)
                    };
                    macro_rules! function {
                        ($name:literal) => {
                            std::mem::transmute::<Pointer, _>(symbol($name)?)
                        };
                    }
                    Some(OpenSsl {
                        method: function!("TLS_client_method"),
                        context_new: function!("SSL_CTX_new"),
                        context_free: function!("SSL_CTX_free"),
                        default_paths: function!("SSL_CTX_set_default_verify_paths"),
                        set_verify: function!("SSL_CTX_set_verify"),
                        new: function!("SSL_new"),
                        free: function!("SSL_free"),
                        set_host: function!("SSL_set1_host"),
                        control: function!("SSL_ctrl"),
                        parameters: function!("SSL_get0_param"),
                        set_ip: function!("X509_VERIFY_PARAM_set1_ip_asc"),
                        set_bio: function!("SSL_set_bio"),
                        connect_state: function!("SSL_set_connect_state"),
                        handshake: function!("SSL_do_handshake"),
                        error: function!("SSL_get_error"),
                        read: function!("SSL_read"),
                        write: function!("SSL_write"),
                        shutdown: function!("SSL_shutdown"),
                        verify_result: function!("SSL_get_verify_result"),
                        bio_memory: function!("BIO_s_mem"),
                        bio_new: function!("BIO_new"),
                        bio_read: function!("BIO_read"),
                        bio_write: function!("BIO_write"),
                        next_error: function!("ERR_get_error"),
                        error_text: function!("ERR_error_string_n"),
                        clear_errors: function!("ERR_clear_error"),
                    })
                }
            })
            .as_ref()
            .ok_or_else(|| {
                (
                    "TlsError",
                    "TLS is not available: OpenSSL's libssl could not be loaded".to_string(),
                )
            })
    }

    pub(super) enum Decrypted {
        More,
        Closed,
        /// OpenSSL takes a new handshake in its stride, inside its reads.
        #[allow(dead_code)]
        HandshakeAgain,
    }

    /// An OpenSSL session over two memory buffers: what arrived goes into
    /// one, and what to send comes out of the other.
    pub(super) struct Session {
        context: Pointer,
        ssl: Pointer,
        incoming: Pointer,
        outgoing: Pointer,
        name: String,
    }

    // SAFETY: a session is used by one thread at a time, inside its
    // connection's entry of the stream table.
    unsafe impl Send for Session {}

    impl Session {
        pub(super) fn client(server: &str) -> Result<Session, HostError> {
            let ssl_lib = openssl()?;
            let name = CString::new(server)
                .map_err(|_| ("TlsError", format!("{server} is not a server's name")))?;
            // SAFETY: OpenSSL's documented calls, on objects made here.
            unsafe {
                (ssl_lib.clear_errors)();
                let context = (ssl_lib.context_new)((ssl_lib.method)());
                if context.is_null() {
                    return Err(failure(ssl_lib, server, "TLS could not start"));
                }
                (ssl_lib.default_paths)(context);
                (ssl_lib.set_verify)(context, SSL_VERIFY_PEER, std::ptr::null_mut());
                let ssl = (ssl_lib.new)(context);
                if ssl.is_null() {
                    (ssl_lib.context_free)(context);
                    return Err(failure(ssl_lib, server, "TLS could not start"));
                }
                // The name the certificate must be for; a numeric address
                // is checked as one, and is not sent as a name.
                if server.parse::<std::net::IpAddr>().is_ok() {
                    (ssl_lib.set_ip)((ssl_lib.parameters)(ssl), name.as_ptr());
                } else {
                    (ssl_lib.set_host)(ssl, name.as_ptr());
                    (ssl_lib.control)(
                        ssl,
                        SSL_CTRL_SET_TLSEXT_HOSTNAME,
                        TLSEXT_NAMETYPE_HOST_NAME,
                        name.as_ptr() as Pointer,
                    );
                }
                let incoming = (ssl_lib.bio_new)((ssl_lib.bio_memory)());
                let outgoing = (ssl_lib.bio_new)((ssl_lib.bio_memory)());
                (ssl_lib.set_bio)(ssl, incoming, outgoing);
                (ssl_lib.connect_state)(ssl);
                Ok(Session {
                    context,
                    ssl,
                    incoming,
                    outgoing,
                    name: server.to_string(),
                })
            }
        }

        /// Gives OpenSSL what has arrived.
        fn feed(&mut self, ssl_lib: &OpenSsl, incoming: &mut Vec<u8>) {
            let mut written = 0;
            while written < incoming.len() {
                let part = &incoming[written..];
                // SAFETY: the bytes outlive the call.
                let count = unsafe {
                    (ssl_lib.bio_write)(self.incoming, part.as_ptr().cast(), part.len() as c_int)
                };
                if count <= 0 {
                    break;
                }
                written += count as usize;
            }
            incoming.drain(..written);
        }

        /// Takes what OpenSSL has to send.
        fn drain(&mut self, ssl_lib: &OpenSsl, output: &mut Vec<u8>) {
            let mut buffer = [0u8; 16 * 1024];
            loop {
                // SAFETY: the buffer has room for what is asked.
                let count = unsafe {
                    (ssl_lib.bio_read)(
                        self.outgoing,
                        buffer.as_mut_ptr().cast(),
                        buffer.len() as c_int,
                    )
                };
                if count <= 0 {
                    return;
                }
                output.extend_from_slice(&buffer[..count as usize]);
            }
        }

        pub(super) fn handshake(
            &mut self,
            incoming: &mut Vec<u8>,
            output: &mut Vec<u8>,
        ) -> Result<Step, HostError> {
            let ssl_lib = openssl()?;
            self.feed(ssl_lib, incoming);
            // SAFETY: the session's own object.
            let result = unsafe { (ssl_lib.handshake)(self.ssl) };
            self.drain(ssl_lib, output);
            if result == 1 {
                return Ok(Step::Done);
            }
            // SAFETY: as above.
            match unsafe { (ssl_lib.error)(self.ssl, result) } {
                SSL_ERROR_WANT_READ | SSL_ERROR_WANT_WRITE => Ok(Step::NeedMore),
                _ => Err(self.handshake_failure(ssl_lib)),
            }
        }

        pub(super) fn decrypt(
            &mut self,
            incoming: &mut Vec<u8>,
            plain: &mut Vec<u8>,
            output: &mut Vec<u8>,
        ) -> Result<Decrypted, HostError> {
            let ssl_lib = openssl()?;
            self.feed(ssl_lib, incoming);
            let mut buffer = [0u8; 16 * 1024];
            loop {
                // SAFETY: the buffer has room for what is asked.
                let count = unsafe {
                    (ssl_lib.read)(self.ssl, buffer.as_mut_ptr().cast(), buffer.len() as c_int)
                };
                if count > 0 {
                    plain.extend_from_slice(&buffer[..count as usize]);
                    continue;
                }
                // Reading can answer the other side, as TLS 1.3 does to a
                // new key.
                self.drain(ssl_lib, output);
                // SAFETY: the session's own object.
                return match unsafe { (ssl_lib.error)(self.ssl, count) } {
                    SSL_ERROR_WANT_READ | SSL_ERROR_WANT_WRITE => Ok(Decrypted::More),
                    SSL_ERROR_ZERO_RETURN => Ok(Decrypted::Closed),
                    _ => Err(failure(
                        ssl_lib,
                        &self.name,
                        "what came could not be decrypted",
                    )),
                };
            }
        }

        pub(super) fn encrypt(
            &mut self,
            bytes: &[u8],
            output: &mut Vec<u8>,
        ) -> Result<(), HostError> {
            let ssl_lib = openssl()?;
            let mut written = 0;
            while written < bytes.len() {
                let part = &bytes[written..];
                let length = part.len().min(1 << 30) as c_int;
                // SAFETY: the bytes outlive the call; a memory buffer takes
                // all of them.
                let count = unsafe { (ssl_lib.write)(self.ssl, part.as_ptr().cast(), length) };
                if count <= 0 {
                    return Err(failure(
                        ssl_lib,
                        &self.name,
                        "what was written could not be encrypted",
                    ));
                }
                written += count as usize;
                self.drain(ssl_lib, output);
            }
            Ok(())
        }

        pub(super) fn close(&mut self, output: &mut Vec<u8>) {
            if let Ok(ssl_lib) = openssl() {
                // SAFETY: the session's own object.
                unsafe { (ssl_lib.shutdown)(self.ssl) };
                self.drain(ssl_lib, output);
            }
        }

        /// Why the handshake failed: the certificate, said as on Windows,
        /// or the other side.
        fn handshake_failure(&self, ssl_lib: &OpenSsl) -> HostError {
            let name = &self.name;
            // SAFETY: the session's own object.
            let verified = unsafe { (ssl_lib.verify_result)(self.ssl) };
            let message = match verified {
                0 => {
                    let reason = reason(ssl_lib);
                    if reason.contains("wrong version number")
                        || reason.contains("packet length too long")
                        || reason.contains("record layer failure")
                        || reason.contains("unexpected eof")
                    {
                        format!("{name} did not answer with TLS")
                    } else if reason.contains("handshake failure")
                        || reason.contains("unsupported protocol")
                        || reason.contains("no protocols available")
                    {
                        format!("{name} and this computer have no version of TLS in common")
                    } else {
                        format!("TLS with {name} failed: {reason}")
                    }
                }
                9 | 10 => format!("the certificate of {name} has expired, or is not valid yet"),
                62 | 64 => format!("the certificate {name} gave is for another name"),
                23 => format!("the certificate of {name} was revoked"),
                2 | 18 | 19 | 20 | 21 | 27 => {
                    format!(
                        "the certificate of {name} is not from an authority this computer trusts"
                    )
                }
                other => format!("the certificate of {name} could not be checked (error {other})"),
            };
            // SAFETY: clears this thread's queue of errors.
            unsafe { (ssl_lib.clear_errors)() };
            ("TlsError", message)
        }
    }

    impl Drop for Session {
        fn drop(&mut self) {
            if let Ok(ssl_lib) = openssl() {
                // SAFETY: objects of this session, freed once; freeing the
                // session frees its buffers.
                unsafe {
                    (ssl_lib.free)(self.ssl);
                    (ssl_lib.context_free)(self.context);
                }
            }
        }
    }

    /// OpenSSL's reason for the last failure on this thread.
    fn reason(ssl_lib: &OpenSsl) -> String {
        let mut last = String::new();
        loop {
            // SAFETY: takes the oldest error of this thread's queue.
            let code = unsafe { (ssl_lib.next_error)() };
            if code == 0 {
                return last;
            }
            let mut text = [0 as c_char; 256];
            // SAFETY: the buffer's length bounds the text.
            unsafe {
                (ssl_lib.error_text)(code, text.as_mut_ptr(), text.len());
                last = CStr::from_ptr(text.as_ptr()).to_string_lossy().into_owned();
            }
        }
    }

    fn failure(ssl_lib: &OpenSsl, name: &str, what: &str) -> HostError {
        let reason = reason(ssl_lib);
        if reason.is_empty() {
            ("TlsError", format!("TLS with {name} failed: {what}"))
        } else {
            (
                "TlsError",
                format!("TLS with {name} failed: {what} ({reason})"),
            )
        }
    }
}

/// Android: the platform's own TLS, Java's `SSLEngine` (Conscrypt), which
/// trusts what the system and the user trust. Apps cannot load the
/// system's C library for TLS, so the runtime calls into Java (see `jni`);
/// an engine works on buffers as a session here does, never touching the
/// socket. A program run from `adb shell` has no Java VM, so no TLS.
#[cfg(target_os = "android")]
mod platform {
    use super::Step;
    use crate::host::HostError;
    use crate::jni::{Env, Method, Object, Thrown, Value, env};
    use std::sync::OnceLock;

    pub(super) enum Decrypted {
        More,
        Closed,
        /// The engine takes a new handshake in its stride, inside unwrap.
        #[allow(dead_code)]
        HandshakeAgain,
    }

    /// The classes and methods the session calls, found once; the classes
    /// are global references.
    struct Java {
        context_class: Object,
        default_context: Method,
        create_engine: Method,
        use_client_mode: Method,
        parameters: Method,
        identify_endpoint: Method,
        set_parameters: Method,
        begin_handshake: Method,
        handshake_status: Method,
        delegated_task: Method,
        run: Method,
        wrap: Method,
        unwrap: Method,
        close_outbound: Method,
        session: Method,
        packet_size: Method,
        application_size: Method,
        status: Method,
        result_handshake_status: Method,
        consumed: Method,
        produced: Method,
        ordinal: Method,
        buffer_class: Object,
        allocate_direct: Method,
        clear: Method,
        limit: Method,
    }

    // SAFETY: global references and method identities, which any thread
    // attached to the VM may use.
    unsafe impl Send for Java {}
    unsafe impl Sync for Java {}

    fn java(env: Env) -> Result<&'static Java, HostError> {
        static JAVA: OnceLock<Option<Java>> = OnceLock::new();
        JAVA.get_or_init(|| {
            env.frame(|| {
                let class = |name: &str| {
                    let class = env.find_class(name);
                    (env.check().is_ok() && !class.is_null()).then_some(class)
                };
                let context = class("javax/net/ssl/SSLContext")?;
                let engine = class("javax/net/ssl/SSLEngine")?;
                let parameters = class("javax/net/ssl/SSLParameters")?;
                let session = class("javax/net/ssl/SSLSession")?;
                let result = class("javax/net/ssl/SSLEngineResult")?;
                let runnable = class("java/lang/Runnable")?;
                let enumeration = class("java/lang/Enum")?;
                let byte_buffer = class("java/nio/ByteBuffer")?;
                let buffer = class("java/nio/Buffer")?;
                let status = "()Ljavax/net/ssl/SSLEngineResult$HandshakeStatus;";
                let exchange =
                    "(Ljava/nio/ByteBuffer;Ljava/nio/ByteBuffer;)Ljavax/net/ssl/SSLEngineResult;";
                let found = Java {
                    context_class: env.global(context),
                    default_context: env.static_method(
                        context,
                        "getDefault",
                        "()Ljavax/net/ssl/SSLContext;",
                    ),
                    create_engine: env.method(
                        context,
                        "createSSLEngine",
                        "(Ljava/lang/String;I)Ljavax/net/ssl/SSLEngine;",
                    ),
                    use_client_mode: env.method(engine, "setUseClientMode", "(Z)V"),
                    parameters: env.method(
                        engine,
                        "getSSLParameters",
                        "()Ljavax/net/ssl/SSLParameters;",
                    ),
                    identify_endpoint: env.method(
                        parameters,
                        "setEndpointIdentificationAlgorithm",
                        "(Ljava/lang/String;)V",
                    ),
                    set_parameters: env.method(
                        engine,
                        "setSSLParameters",
                        "(Ljavax/net/ssl/SSLParameters;)V",
                    ),
                    begin_handshake: env.method(engine, "beginHandshake", "()V"),
                    handshake_status: env.method(engine, "getHandshakeStatus", status),
                    delegated_task: env.method(
                        engine,
                        "getDelegatedTask",
                        "()Ljava/lang/Runnable;",
                    ),
                    run: env.method(runnable, "run", "()V"),
                    wrap: env.method(engine, "wrap", exchange),
                    unwrap: env.method(engine, "unwrap", exchange),
                    close_outbound: env.method(engine, "closeOutbound", "()V"),
                    session: env.method(engine, "getSession", "()Ljavax/net/ssl/SSLSession;"),
                    packet_size: env.method(session, "getPacketBufferSize", "()I"),
                    application_size: env.method(session, "getApplicationBufferSize", "()I"),
                    status: env.method(
                        result,
                        "getStatus",
                        "()Ljavax/net/ssl/SSLEngineResult$Status;",
                    ),
                    result_handshake_status: env.method(result, "getHandshakeStatus", status),
                    consumed: env.method(result, "bytesConsumed", "()I"),
                    produced: env.method(result, "bytesProduced", "()I"),
                    ordinal: env.method(enumeration, "ordinal", "()I"),
                    buffer_class: env.global(byte_buffer),
                    allocate_direct: env.static_method(
                        byte_buffer,
                        "allocateDirect",
                        "(I)Ljava/nio/ByteBuffer;",
                    ),
                    clear: env.method(buffer, "clear", "()Ljava/nio/Buffer;"),
                    limit: env.method(buffer, "limit", "(I)Ljava/nio/Buffer;"),
                };
                env.check().ok().map(|()| found)
            })
        })
        .as_ref()
        .ok_or_else(|| ("TlsError", "Android's TLS could not be found".to_string()))
    }

    /// `SSLEngineResult.Status`, by its order.
    const BUFFER_UNDERFLOW: i32 = 0;
    const CLOSED: i32 = 3;
    /// `SSLEngineResult.HandshakeStatus`, by its order.
    const NEED_TASK: i32 = 2;
    const NEED_WRAP: i32 = 3;
    const NEED_UNWRAP: i32 = 4;
    const NEED_UNWRAP_AGAIN: i32 = 5;

    /// A direct byte buffer of Java's, whose bytes the runtime reads and
    /// writes in place.
    struct Buffer {
        object: Object,
        address: *mut u8,
        capacity: usize,
    }

    /// What a wrap or an unwrap did.
    struct Outcome {
        status: i32,
        handshake: i32,
        consumed: usize,
        produced: usize,
    }

    pub(super) struct Session {
        engine: Object,
        /// What arrived, what goes out, what was decrypted, and what is to
        /// be encrypted.
        network_in: Buffer,
        network_out: Buffer,
        plain_in: Buffer,
        plain_out: Buffer,
        /// What the handshake decrypted already, for the first read.
        early: Vec<u8>,
        started: bool,
        name: String,
    }

    // SAFETY: global references, used by one thread at a time, inside the
    // connection's entry of the stream table, each attached to the VM.
    unsafe impl Send for Session {}

    fn no_java() -> HostError {
        (
            "TlsError",
            "TLS on Android comes from its Java libraries, which only an app has; a program \
             run from a shell has none"
                .to_string(),
        )
    }

    fn env_or_fail() -> Result<Env, HostError> {
        env().ok_or_else(no_java)
    }

    impl Session {
        pub(super) fn client(server: &str) -> Result<Session, HostError> {
            let env = env_or_fail()?;
            let java = java(env)?;
            let name = server.to_string();
            let failed = |thrown: Thrown| failure(&name, &thrown);
            env.frame(|| {
                let context = env.call_static_object(java.context_class, java.default_context, &[]);
                env.check().map_err(failed)?;
                let host = env.new_string(server);
                let engine = env.call_object(
                    context,
                    java.create_engine,
                    &[Value { l: host }, Value { i: 443 }],
                );
                env.check().map_err(failed)?;
                env.call_void(engine, java.use_client_mode, &[Value { z: 1 }]);
                // The certificate must be for the name, as for HTTPS.
                let parameters = env.call_object(engine, java.parameters, &[]);
                let https = env.new_string("HTTPS");
                env.call_void(parameters, java.identify_endpoint, &[Value { l: https }]);
                env.call_void(engine, java.set_parameters, &[Value { l: parameters }]);
                env.check().map_err(failed)?;
                let session = env.call_object(engine, java.session, &[]);
                let packet = env.call_int(session, java.packet_size, &[]).max(16 * 1024) as usize;
                let plain = env
                    .call_int(session, java.application_size, &[])
                    .max(16 * 1024) as usize;
                env.check().map_err(failed)?;
                let buffer = |capacity: usize| -> Result<Buffer, HostError> {
                    let object = env.call_static_object(
                        java.buffer_class,
                        java.allocate_direct,
                        &[Value { i: capacity as i32 }],
                    );
                    env.check().map_err(failed)?;
                    let address = env.buffer_address(object);
                    Ok(Buffer {
                        object: env.global(object),
                        address,
                        capacity,
                    })
                };
                Ok(Session {
                    engine: env.global(engine),
                    network_in: buffer(packet * 2)?,
                    network_out: buffer(packet * 2)?,
                    plain_in: buffer(plain * 2)?,
                    plain_out: buffer(plain)?,
                    early: Vec::new(),
                    started: false,
                    name: server.to_string(),
                })
            })
        }

        /// Makes a buffer hold `length` bytes from its start.
        fn fill(&self, env: Env, java: &Java, buffer: &Buffer, bytes: &[u8]) -> usize {
            let length = bytes.len().min(buffer.capacity);
            // SAFETY: the buffer has room for `length` bytes.
            unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), buffer.address, length) };
            env.call_object(buffer.object, java.clear, &[]);
            env.call_object(buffer.object, java.limit, &[Value { i: length as i32 }]);
            length
        }

        /// Wraps (`true`) or unwraps what `source` holds into `target`, both
        /// from their starts.
        fn exchange(
            &self,
            env: Env,
            java: &Java,
            wrap: bool,
            source: &Buffer,
            target: &Buffer,
        ) -> Result<Outcome, HostError> {
            env.call_object(target.object, java.clear, &[]);
            let method = if wrap { java.wrap } else { java.unwrap };
            let result = env.call_object(
                self.engine,
                method,
                &[Value { l: source.object }, Value { l: target.object }],
            );
            env.check().map_err(|thrown| failure(&self.name, &thrown))?;
            let status = env.call_object(result, java.status, &[]);
            let handshake = env.call_object(result, java.result_handshake_status, &[]);
            Ok(Outcome {
                status: env.call_int(status, java.ordinal, &[]),
                handshake: env.call_int(handshake, java.ordinal, &[]),
                consumed: env.call_int(result, java.consumed, &[]).max(0) as usize,
                produced: env.call_int(result, java.produced, &[]).max(0) as usize,
            })
        }

        /// What a buffer holds from its start.
        fn take(buffer: &Buffer, length: usize, into: &mut Vec<u8>) {
            // SAFETY: the engine put `length` bytes there.
            unsafe {
                into.extend_from_slice(std::slice::from_raw_parts(
                    buffer.address,
                    length.min(buffer.capacity),
                ))
            };
        }

        /// Wraps nothing, for the engine's own messages, into `output`.
        fn wrap_own(
            &self,
            env: Env,
            java: &Java,
            output: &mut Vec<u8>,
        ) -> Result<Outcome, HostError> {
            self.fill(env, java, &self.plain_out, &[]);
            let outcome = self.exchange(env, java, true, &self.plain_out, &self.network_out)?;
            Self::take(&self.network_out, outcome.produced, output);
            Ok(outcome)
        }

        /// Unwraps what has arrived, a record at a time, the plain bytes
        /// into `plain`. Gives the last outcome, and whether more is needed.
        fn unwrap_some(
            &self,
            env: Env,
            java: &Java,
            incoming: &mut Vec<u8>,
            plain: &mut Vec<u8>,
        ) -> Result<Option<Outcome>, HostError> {
            let mut last = None;
            while !incoming.is_empty() {
                self.fill(env, java, &self.network_in, incoming);
                let outcome = self.exchange(env, java, false, &self.network_in, &self.plain_in)?;
                incoming.drain(..outcome.consumed.min(incoming.len()));
                Self::take(&self.plain_in, outcome.produced, plain);
                let stop = outcome.status == BUFFER_UNDERFLOW
                    || outcome.status == CLOSED
                    || (outcome.consumed == 0 && outcome.produced == 0)
                    || outcome.handshake == NEED_WRAP
                    || outcome.handshake == NEED_TASK;
                last = Some(outcome);
                if stop {
                    break;
                }
            }
            Ok(last)
        }

        fn run_tasks(&self, env: Env, java: &Java) -> Result<(), HostError> {
            loop {
                let task = env.call_object(self.engine, java.delegated_task, &[]);
                env.check().map_err(|thrown| failure(&self.name, &thrown))?;
                if task.is_null() {
                    return Ok(());
                }
                env.call_void(task, java.run, &[]);
                env.check().map_err(|thrown| failure(&self.name, &thrown))?;
            }
        }

        fn status(&self, env: Env, java: &Java) -> Result<i32, HostError> {
            let status = env.call_object(self.engine, java.handshake_status, &[]);
            env.check().map_err(|thrown| failure(&self.name, &thrown))?;
            Ok(env.call_int(status, java.ordinal, &[]))
        }

        pub(super) fn handshake(
            &mut self,
            incoming: &mut Vec<u8>,
            output: &mut Vec<u8>,
        ) -> Result<Step, HostError> {
            let env = env_or_fail()?;
            let java = java(env)?;
            env.frame(|| {
                if !std::mem::replace(&mut self.started, true) {
                    env.call_void(self.engine, java.begin_handshake, &[]);
                    env.check().map_err(|thrown| failure(&self.name, &thrown))?;
                }
                loop {
                    match self.status(env, java)? {
                        NEED_WRAP => {
                            self.wrap_own(env, java, output)?;
                        }
                        NEED_UNWRAP | NEED_UNWRAP_AGAIN => {
                            let mut early = std::mem::take(&mut self.early);
                            let outcome = self.unwrap_some(env, java, incoming, &mut early)?;
                            self.early = early;
                            match outcome {
                                None => return Ok(Step::NeedMore),
                                Some(outcome) if outcome.status == BUFFER_UNDERFLOW => {
                                    return Ok(Step::NeedMore);
                                }
                                Some(outcome) if outcome.status == CLOSED => {
                                    return Err((
                                        "TlsError",
                                        format!("{} closed before TLS started", self.name),
                                    ));
                                }
                                Some(_) => {}
                            }
                        }
                        NEED_TASK => self.run_tasks(env, java)?,
                        // Finished, or not handshaking at all.
                        _ => return Ok(Step::Done),
                    }
                }
            })
        }

        pub(super) fn decrypt(
            &mut self,
            incoming: &mut Vec<u8>,
            plain: &mut Vec<u8>,
            output: &mut Vec<u8>,
        ) -> Result<Decrypted, HostError> {
            let env = env_or_fail()?;
            let java = java(env)?;
            plain.append(&mut self.early);
            env.frame(|| {
                loop {
                    let outcome = self.unwrap_some(env, java, incoming, plain)?;
                    // Reading can need an answer, as TLS 1.3 gives a new key.
                    match self.status(env, java)? {
                        NEED_WRAP => {
                            self.wrap_own(env, java, output)?;
                            continue;
                        }
                        NEED_TASK => {
                            self.run_tasks(env, java)?;
                            continue;
                        }
                        _ => {}
                    }
                    return Ok(match outcome {
                        Some(outcome) if outcome.status == CLOSED => Decrypted::Closed,
                        _ => Decrypted::More,
                    });
                }
            })
        }

        pub(super) fn encrypt(
            &mut self,
            bytes: &[u8],
            output: &mut Vec<u8>,
        ) -> Result<(), HostError> {
            let env = env_or_fail()?;
            let java = java(env)?;
            env.frame(|| {
                let mut written = 0;
                while written < bytes.len() {
                    let length = self.fill(env, java, &self.plain_out, &bytes[written..]);
                    let outcome =
                        self.exchange(env, java, true, &self.plain_out, &self.network_out)?;
                    Self::take(&self.network_out, outcome.produced, output);
                    if outcome.consumed == 0 && length > 0 {
                        return Err((
                            "TlsError",
                            format!(
                                "TLS with {} failed: what was written could not be encrypted",
                                self.name
                            ),
                        ));
                    }
                    written += outcome.consumed;
                }
                Ok(())
            })
        }

        pub(super) fn close(&mut self, output: &mut Vec<u8>) {
            let Some(env) = env() else {
                return;
            };
            let Ok(java) = java(env) else {
                return;
            };
            env.frame(|| {
                env.call_void(self.engine, java.close_outbound, &[]);
                let _ = env.thrown();
                for _ in 0..4 {
                    match self.wrap_own(env, java, output) {
                        Ok(outcome) if outcome.status != CLOSED && outcome.produced > 0 => continue,
                        _ => break,
                    }
                }
            });
        }
    }

    impl Drop for Session {
        fn drop(&mut self) {
            if let Some(env) = env() {
                for object in [
                    self.engine,
                    self.network_in.object,
                    self.network_out.object,
                    self.plain_in.object,
                    self.plain_out.object,
                ] {
                    env.drop_global(object);
                }
            }
        }
    }

    /// Why TLS failed, said as on the other systems.
    fn failure(name: &str, thrown: &Thrown) -> HostError {
        let text = &thrown.0;
        let has = |part: &str| {
            text.to_ascii_lowercase()
                .contains(&part.to_ascii_lowercase())
        };
        let message = if has("CertificateExpiredException")
            || has("CertificateNotYetValidException")
            || has("expired")
        {
            format!("the certificate of {name} has expired, or is not valid yet")
        } else if has("subjectAltNames") || has("not verified") {
            format!("the certificate {name} gave is for another name")
        } else if has("revoked") {
            format!("the certificate of {name} was revoked")
        } else if has("Trust anchor") || has("CertPathValidatorException") {
            format!("the certificate of {name} is not from an authority this computer trusts")
        } else if has("Unable to parse TLS packet header")
            || has("WRONG_VERSION_NUMBER")
            || has("unrecognized SSL message")
            || has("NOT_SSL")
            || has("Unsupported or unrecognized")
        {
            format!("{name} did not answer with TLS")
        } else if has("UNSUPPORTED_PROTOCOL")
            || has("NO_PROTOCOLS_AVAILABLE")
            || has("HANDSHAKE_FAILURE")
        {
            format!("{name} and this computer have no version of TLS in common")
        } else {
            format!("TLS with {name} failed: {text}")
        };
        ("TlsError", message)
    }
}

#[cfg(not(any(windows, unix)))]
mod platform {
    use super::Step;
    use crate::host::HostError;

    #[allow(dead_code)]
    pub(super) enum Decrypted {
        More,
        Closed,
        HandshakeAgain,
    }

    pub(super) struct Session;

    impl Session {
        pub(super) fn client(_server: &str) -> Result<Session, HostError> {
            Err((
                "TlsError",
                "TLS is not available on this system yet".to_string(),
            ))
        }

        pub(super) fn handshake(
            &mut self,
            _incoming: &mut Vec<u8>,
            _output: &mut Vec<u8>,
        ) -> Result<Step, HostError> {
            unreachable!("no session starts on this system")
        }

        pub(super) fn decrypt(
            &mut self,
            _incoming: &mut Vec<u8>,
            _plain: &mut Vec<u8>,
            _output: &mut Vec<u8>,
        ) -> Result<Decrypted, HostError> {
            unreachable!("no session starts on this system")
        }

        pub(super) fn encrypt(
            &mut self,
            _bytes: &[u8],
            _output: &mut Vec<u8>,
        ) -> Result<(), HostError> {
            unreachable!("no session starts on this system")
        }

        pub(super) fn close(&mut self, _output: &mut Vec<u8>) {}
    }
}
