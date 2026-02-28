use std::borrow::Cow;
use std::collections::HashSet;

use ironrdp_core::{cast_length, impl_as_any, Decode as _, EncodeResult, ReadCursor};
use ironrdp_pdu::gcc::ChannelName;
use ironrdp_pdu::{decode_err, encode_err, pdu_other_err, PduResult};
use ironrdp_svc::{CompressionCondition, SvcClientProcessor, SvcMessage, SvcProcessor};
use tracing::{debug, error, info, warn};

use crate::pdu::{self, AudioFormat, PitchPdu, ServerAudioFormatPdu, TrainingPdu, VolumePdu};
use crate::server::RdpsndSvcMessages;

pub trait RdpsndClientHandler: Send + core::fmt::Debug {
    fn get_flags(&self) -> pdu::AudioFormatFlags {
        pdu::AudioFormatFlags::empty()
    }

    fn get_formats(&self) -> &[AudioFormat];

    fn wave(&mut self, format_no: usize, ts: u32, data: Cow<'_, [u8]>);

    fn set_volume(&mut self, volume: VolumePdu);

    fn set_pitch(&mut self, pitch: PitchPdu);

    fn close(&mut self);
}

#[derive(Debug)]
pub struct NoopRdpsndBackend;

impl RdpsndClientHandler for NoopRdpsndBackend {
    fn get_formats(&self) -> &[AudioFormat] {
        &[]
    }

    fn wave(&mut self, _format_no: usize, _ts: u32, _data: Cow<'_, [u8]>) {}

    fn set_volume(&mut self, _volume: VolumePdu) {}

    fn set_pitch(&mut self, _pitch: PitchPdu) {}

    fn close(&mut self) {}
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum RdpsndState {
    Start,
    WaitingForTraining,
    Ready,
    Stop,
}

/// Required for rdpdr to work: [\[MS-RDPEFS\] Appendix A<1>]
///
/// [\[MS-RDPEFS\] Appendix A<1>]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpefs/fd28bfd9-dae2-4a78-abe1-b4efa208b7aa#Appendix_A_1
#[derive(Debug)]
pub struct Rdpsnd {
    handler: Box<dyn RdpsndClientHandler>,
    state: RdpsndState,
    server_format: Option<ServerAudioFormatPdu>,
    /// The negotiated format list sent to the server in the Client Audio Formats PDU.
    /// The server's `format_no` in Wave/Wave2 PDUs references indices in this list.
    negotiated_formats: Vec<AudioFormat>,
}

impl Rdpsnd {
    pub const NAME: ChannelName = ChannelName::from_static(b"rdpsnd\0\0");

    pub fn new(handler: Box<dyn RdpsndClientHandler>) -> Self {
        Self {
            handler,
            state: RdpsndState::Start,
            server_format: None,
            negotiated_formats: Vec::new(),
        }
    }

    pub fn get_format(&self, format_no: u16) -> PduResult<&AudioFormat> {
        let server_format = self
            .server_format
            .as_ref()
            .ok_or_else(|| pdu_other_err!("invalid state - no format"))?;

        server_format
            .formats
            .get(usize::from(format_no))
            .ok_or_else(|| pdu_other_err!("invalid format"))
    }

    pub fn version(&self) -> PduResult<pdu::Version> {
        let server_format = self
            .server_format
            .as_ref()
            .ok_or_else(|| pdu_other_err!("invalid state - no version"))?;

        Ok(server_format.version)
    }

    pub fn client_formats(&mut self) -> PduResult<RdpsndSvcMessages> {
        // Windows seems to be confused if the client replies with more formats, or unknown formats (e.g.: opus).
        // We ensure to only send supported formats in common with the server.
        let server_format: HashSet<_> = self
            .server_format
            .as_ref()
            .ok_or_else(|| pdu_other_err!("invalid state - no server format"))?
            .formats
            .iter()
            .collect();
        let client_formats_set: HashSet<_> = self.handler.get_formats().iter().collect();
        let negotiated_formats: Vec<_> = client_formats_set.intersection(&server_format).map(|&x| x.clone()).collect();

        info!("Negotiated {} audio formats in common with server", negotiated_formats.len());
        for (i, format) in negotiated_formats.iter().enumerate() {
            info!("Negotiated format {}: {:?}", i, format);
        }

        if negotiated_formats.is_empty() {
            warn!("No common audio formats found! Server offered: {:?}", server_format);
            warn!("Client supports: {:?}", client_formats_set);
        }

        // Store the negotiated formats so we can resolve format_no from Wave/Wave2 PDUs.
        // The server's format_no references indices in this negotiated list.
        self.negotiated_formats = negotiated_formats.clone();

        let pdu = pdu::ClientAudioFormatPdu {
            version: self.version()?,
            flags: self.handler.get_flags() | pdu::AudioFormatFlags::ALIVE,
            formats: negotiated_formats,
            volume_left: 0xFFFF,
            volume_right: 0xFFFF,
            pitch: 0x00010000,
            dgram_port: 0,
        };
        Ok(RdpsndSvcMessages::new(vec![pdu::ClientAudioOutputPdu::AudioFormat(
            pdu,
        )
        .into()]))
    }

    pub fn quality_mode(&mut self) -> PduResult<RdpsndSvcMessages> {
        let pdu = pdu::QualityModePdu {
            quality_mode: pdu::QualityMode::High,
        };
        Ok(RdpsndSvcMessages::new(vec![pdu::ClientAudioOutputPdu::QualityMode(
            pdu,
        )
        .into()]))
    }

    pub fn training_confirm(&mut self, pdu: &TrainingPdu) -> PduResult<RdpsndSvcMessages> {
        let pack_size: EncodeResult<_> = cast_length!("wPackSize", pdu.data.len());
        let pack_size = pack_size.map_err(|e| encode_err!(e))?;
        let pdu = pdu::TrainingConfirmPdu {
            timestamp: pdu.timestamp,
            pack_size,
        };
        Ok(RdpsndSvcMessages::new(vec![
            pdu::ClientAudioOutputPdu::TrainingConfirm(pdu).into(),
        ]))
    }

    pub fn wave_confirm(&mut self, timestamp: u16, block_no: u8) -> PduResult<RdpsndSvcMessages> {
        let pdu = pdu::WaveConfirmPdu { timestamp, block_no };
        Ok(RdpsndSvcMessages::new(vec![pdu::ClientAudioOutputPdu::WaveConfirm(
            pdu,
        )
        .into()]))
    }

    /// Resolve the server's format_no (index into the negotiated format list) to the
    /// corresponding index in the handler's supported format list.
    ///
    /// The server sends format_no as an index into the negotiated formats that the client
    /// sent in the Client Audio Formats PDU. We need to find the matching format in the
    /// handler's supported formats list so it can decode the audio data correctly.
    fn resolve_handler_format_no(&self, server_format_no: usize) -> usize {
        // Look up the actual format from the negotiated list
        if let Some(negotiated_format) = self.negotiated_formats.get(server_format_no) {
            // Find the index of this format in the handler's supported formats
            if let Some(handler_idx) = self
                .handler
                .get_formats()
                .iter()
                .position(|f| f == negotiated_format)
            {
                return handler_idx;
            }
            warn!(
                "Negotiated format {:?} not found in handler's supported formats, using server format_no {}",
                negotiated_format, server_format_no
            );
        } else {
            warn!(
                "Server format_no {} out of range for negotiated formats (len={}), passing through",
                server_format_no,
                self.negotiated_formats.len()
            );
        }
        // Fallback: pass through the server's format_no directly
        server_format_no
    }
}

impl_as_any!(Rdpsnd);

impl SvcProcessor for Rdpsnd {
    fn channel_name(&self) -> ChannelName {
        Self::NAME
    }

    fn compression_condition(&self) -> CompressionCondition {
        CompressionCondition::Never
    }

    fn process(&mut self, payload: &[u8]) -> PduResult<Vec<SvcMessage>> {
        let pdu = pdu::ServerAudioOutputPdu::decode(&mut ReadCursor::new(payload)).map_err(|e| decode_err!(e))?;

        debug!(?pdu, ?self.state);
        let msg = match self.state {
            RdpsndState::Start => {
                let pdu::ServerAudioOutputPdu::AudioFormat(af) = pdu else {
                    error!("Invalid pdu");
                    self.state = RdpsndState::Stop;
                    return Ok(vec![]);
                };
                self.server_format = Some(af);
                self.state = RdpsndState::WaitingForTraining;
                let mut msgs: Vec<SvcMessage> = self.client_formats()?.into();
                if self.version()? >= pdu::Version::V6 {
                    let mut m = self.quality_mode()?.into();
                    msgs.append(&mut m);
                }
                msgs
            }
            RdpsndState::WaitingForTraining => {
                let pdu::ServerAudioOutputPdu::Training(pdu) = pdu else {
                    error!("Invalid PDU");
                    self.state = RdpsndState::Stop;
                    return Ok(vec![]);
                };
                self.state = RdpsndState::Ready;
                self.training_confirm(&pdu)?.into()
            }
            RdpsndState::Ready => {
                match pdu {
                    // TODO: handle WaveInfo for < v8
                    pdu::ServerAudioOutputPdu::Wave2(pdu) => {
                        let server_format_no = usize::from(pdu.format_no);
                        // Resolve the format_no from the negotiated list to the handler's format list.
                        // The server's format_no is an index into the negotiated format list we sent.
                        let handler_format_no = self.resolve_handler_format_no(server_format_no);
                        let ts = pdu.audio_timestamp;
                        self.handler.wave(handler_format_no, ts, pdu.data);
                        return Ok(self.wave_confirm(pdu.timestamp, pdu.block_no)?.into());
                    }
                    pdu::ServerAudioOutputPdu::Wave(pdu) => {
                        let server_format_no = usize::from(pdu.format_no);
                        // Resolve the format_no from the negotiated list to the handler's format list.
                        let handler_format_no = self.resolve_handler_format_no(server_format_no);
                        let ts = u32::from(pdu.timestamp);
                        self.handler.wave(handler_format_no, ts, pdu.data);
                        return Ok(self.wave_confirm(pdu.timestamp, pdu.block_no)?.into());
                    }
                    pdu::ServerAudioOutputPdu::Volume(pdu) => {
                        self.handler.set_volume(pdu);
                    }
                    pdu::ServerAudioOutputPdu::Pitch(pdu) => {
                        self.handler.set_pitch(pdu);
                    }
                    pdu::ServerAudioOutputPdu::Close => {
                        self.handler.close();
                    }
                    pdu::ServerAudioOutputPdu::Training(pdu) => return Ok(self.training_confirm(&pdu)?.into()),
                    _ => {
                        error!("Invalid PDU");
                        self.state = RdpsndState::Stop;
                        return Ok(vec![]);
                    }
                }
                vec![]
            }
            state => {
                error!(?state, "Invalid state");
                vec![]
            }
        };

        Ok(msg)
    }
}

impl Drop for Rdpsnd {
    fn drop(&mut self) {
        self.handler.close();
    }
}

impl SvcClientProcessor for Rdpsnd {}
