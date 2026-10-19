//! TLS for connections, from the operating system: SChannel on Windows,
//! whose certificate store decides which servers are trusted. The other
//! systems' libraries are still to come.
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
            match self.inner.decrypt(&mut self.incoming, plain)? {
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

#[cfg(not(windows))]
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
            Err(("TlsError", "TLS is available on Windows so far".to_string()))
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
