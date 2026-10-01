use crate::common::*;
use crate::device_server::flows_tx::FlowInfo;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use tokio::sync::Mutex;

use super::flows_tx::{FlowsTransmitter, FPP_MAX, MAX_FLOWS};
use crate::protocol::flows_control::{
  parse_flow_request, parse_flow_update, FlowControlError, FlowRequest,
};
use crate::{device_info::DeviceInfo, net_utils::UdpSocketWrapper, protocol::req_resp};
use tokio::sync::broadcast::Receiver as BroadcastReceiver;

/// Serves flow requests for the device's whole lifetime; while it isn't transmitting, they are refused.
pub async fn run_server(
  self_info: Arc<DeviceInfo>,
  flows_tx: Arc<Mutex<Option<FlowsTransmitter>>>,
  server: UdpSocketWrapper,
) {
  let mut conn = req_resp::Connection::new(server);
  let mut recv_buff = crate::net_utils::ReceiveBuffer::new();
  while conn.should_work() {
    let request = match conn.recv(&mut recv_buff).await {
      Some(v) => v,
      None => continue,
    };

    if request.opcode2().read() == 0 {
      match request.opcode1().read() {
        0x0100 => {
          // request flow
          let packet = request.into_storage();
          let req = match parse_flow_request(packet) {
            Ok(req) => req,
            Err(e) => {
              error!("invalid flow request ({e}): {}", hex::encode(packet));
              continue;
            }
          };
          let FlowRequest { sample_rate, bits_per_sample, fpp, rx_ip, rx_port, .. } = req;
          let channel_indices = req.channel_indices;
          info!(
            "{} requesting flow {} of channel indices {channel_indices:?} at {sample_rate}Hz {bits_per_sample}bit {fpp} fpp to {rx_ip}:{rx_port}",
            req.rx_hostname, req.rx_flow_name
          );
          if channel_indices.iter().flatten().any(|&chi| chi >= self_info.tx_channels.len()) {
            error!("too large channel number, returning error");
            conn.respond_with_code(0x0302u16 /* ??? TODO */, &[]).await;
            continue;
          }
          if sample_rate != self_info.sample_rate {
            error!("sample rate mismatch, returning error");
            conn.respond_with_code(FlowControlError::SampleRateMismatch as u16, &[]).await;
            continue;
          }
          if fpp > FPP_MAX {
            error!("too large fpp, returning error");
            conn.respond_with_code(0x0302u16 /* TODO */, &[]).await;
            continue;
          }
          let flow_info = FlowInfo {
            rx_hostname: Some(req.rx_hostname.to_owned()),
            rx_flow_name: Some(req.rx_flow_name.to_owned()),
            dst_addr: rx_ip,
            dst_port: rx_port,
            local_channel_indices: channel_indices,
          };
          let mut flows_tx = flows_tx.lock().await;
          let Some(flows_tx) = flows_tx.as_mut() else {
            warn!("refusing flow request: not transmitting");
            conn.respond_with_code(FlowControlError::TooManyTXFlows as u16, &[]).await;
            continue;
          };
          let result =
            flows_tx.add_flow(flow_info, fpp as usize, (bits_per_sample / 8) as usize, None, false).await;
          match result {
            Ok((_flow_index, handle)) => {
              conn.respond(&handle).await;
            }
            Err(e) => {
              error!("adding flow failed: {e:?}");
              conn.respond_with_code(FlowControlError::TooManyTXFlows as u16, &[]).await;
            }
          }
        }
        0x0101 => {
          // stop flow
          let handle = if let Ok(handle) = request.content().try_into() {
            handle
          } else {
            error!("packet too short: {}", hex::encode(request.content()));
            continue;
          };
          let removed = match flows_tx.lock().await.as_mut() {
            Some(flows_tx) => flows_tx.remove_flow(handle).await.is_ok(),
            None => false,
          };
          if removed {
            info!("stopped flow {handle:?}");
            conn.respond(&[]).await;
          } else {
            warn!("received stop flow request for unknown handle {handle:?}");
            conn.respond_with_code(FlowControlError::FlowNotFound as u16, &[]).await;
          }
        }
        0x0102 => {
          // update flow
          let (handle, channel_indices) = match parse_flow_update(request.content()) {
            Ok(v) => v,
            Err(e) => {
              error!("invalid update flow request ({e}): {}", hex::encode(request.content()));
              continue;
            }
          };
          if channel_indices.iter().flatten().any(|&chi| chi >= self_info.tx_channels.len()) {
            error!("update flow: too large channel number, returning error");
            conn.respond_with_code(0x0302u16 /* ??? TODO */, &[]).await;
            continue;
          }
          let updated = match flows_tx.lock().await.as_mut() {
            Some(tx) => tx.set_channels(handle, channel_indices.clone()).await.is_ok(),
            None => false,
          };
          if updated {
            info!("set channels {channel_indices:?} in flow {handle:?}");
            conn.respond(&[]).await;
          } else {
            warn!("received update flow request for unknown handle {handle:?}");
            conn.respond_with_code(FlowControlError::FlowNotFound as u16, &[]).await;
          }
        }

        x => {
          error!("received unknown opcode1 {x:#04x}, content {}", hex::encode(request.content()));
          error!("whole packet: {:?}", hex::encode(request.into_storage()));
        }
      }
    } else {
      error!(
        "received unknown opcode2 {:#04x}, content {}",
        request.opcode2().read(),
        hex::encode(request.content())
      );
      error!("whole packet: {:?}", hex::encode(request.into_storage()));
    }
  }
}
