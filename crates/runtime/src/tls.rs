//! TLS for connections, from the operating system: SChannel on Windows,
//! and OpenSSL on Linux and the other Unix systems that have it, loaded
//! when first needed. The system's certificate store decides which servers
//! are trusted.
//!
//! A session never touches the socket. The connection gives it what has
//! arrived, and sends what it puts out, so that the handshake and every
//! read and write can wait for the socket like the rest of a connection's
//! operations, and run again where they left off.

use crate::host::HostError;

/// What a step of the handshake reached.
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

#[cfg(unix)]
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

#[cfg(not(any(windows, unix)))]
mod platform {
    use super::Step;
    use crate::host::HostError;

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
