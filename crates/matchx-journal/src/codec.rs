use crate::JournalError;
use matchx_types::*;

/// Encode one full framed journal record:
/// [u32 payload_len][u64 sequence][payload_bytes][u32 crc32].
pub fn encode_record(sequence: u64, cmd: &Command) -> Vec<u8> {
    let payload = encode(cmd);
    let payload_len = payload.len() as u32;

    // CRC covers sequence bytes followed by the payload bytes.
    let mut crc_input = Vec::with_capacity(8 + payload.len());
    crc_input.extend_from_slice(&sequence.to_le_bytes());
    crc_input.extend_from_slice(&payload);
    let crc = crc32fast::hash(&crc_input);

    let mut framed = Vec::with_capacity(4 + 8 + payload.len() + 4);
    framed.extend_from_slice(&payload_len.to_le_bytes());
    framed.extend_from_slice(&sequence.to_le_bytes());
    framed.extend_from_slice(&payload);
    framed.extend_from_slice(&crc.to_le_bytes());
    framed
}

/// Decode one framed record from the beginning of `data`.
/// Returns `(sequence, command, bytes_consumed)`.
pub fn decode_record(data: &[u8]) -> Result<(u64, Command, usize), JournalError> {
    if data.len() < 4 {
        return Err(JournalError::InvalidData);
    }
    let payload_len = u32::from_le_bytes(data[0..4].try_into().unwrap()) as usize;
    // Reject obviously invalid payload lengths to prevent overflow on 32-bit.
    if payload_len > 1 << 20 {
        return Err(JournalError::InvalidData);
    }
    let required = 4 + 8 + payload_len + 4;
    if data.len() < required {
        return Err(JournalError::InvalidData);
    }

    let sequence = u64::from_le_bytes(data[4..12].try_into().unwrap());
    let payload = &data[12..12 + payload_len];
    let stored_crc = u32::from_le_bytes(
        data[12 + payload_len..12 + payload_len + 4]
            .try_into()
            .unwrap(),
    );

    let mut crc_input = Vec::with_capacity(8 + payload_len);
    crc_input.extend_from_slice(&sequence.to_le_bytes());
    crc_input.extend_from_slice(payload);
    let computed_crc = crc32fast::hash(&crc_input);
    if computed_crc != stored_crc {
        return Err(JournalError::CrcMismatch);
    }

    let cmd = decode(payload)?;
    Ok((sequence, cmd, required))
}

/// Encode a Command into a compact little-endian binary blob.
///
/// Format per variant:
///   NewOrder   : [0u8][u64 id][u32 instrument_id][u8 side][u64 price][u64 qty]
///                [u8 order_type][u8 tif]
///                [u8 visible_tag][u64? visible_qty]
///                [u8 stop_tag][u64? stop_price]
///                [u8 stp_tag][u32? stp_group]
///   CancelOrder: [1u8][u64 id]
///   ModifyOrder: [2u8][u64 id][u64 new_price][u64 new_qty]
pub fn encode(cmd: &Command) -> Vec<u8> {
    let mut buf = Vec::with_capacity(64);
    match cmd {
        Command::NewOrder {
            id,
            instrument_id,
            side,
            price,
            qty,
            order_type,
            time_in_force,
            visible_qty,
            stop_price,
            stp_group,
        } => {
            buf.push(0u8);
            buf.extend_from_slice(&id.0.to_le_bytes());
            buf.extend_from_slice(&instrument_id.to_le_bytes());
            buf.push(*side as u8);
            buf.extend_from_slice(&price.to_le_bytes());
            buf.extend_from_slice(&qty.to_le_bytes());
            buf.push(*order_type as u8);
            buf.push(*time_in_force as u8);
            push_option_u64(&mut buf, *visible_qty);
            push_option_u64(&mut buf, *stop_price);
            push_option_u32(&mut buf, *stp_group);
        }
        Command::CancelOrder { id } => {
            buf.push(1u8);
            buf.extend_from_slice(&id.0.to_le_bytes());
        }
        Command::ModifyOrder {
            id,
            new_price,
            new_qty,
        } => {
            buf.push(2u8);
            buf.extend_from_slice(&id.0.to_le_bytes());
            buf.extend_from_slice(&new_price.to_le_bytes());
            buf.extend_from_slice(&new_qty.to_le_bytes());
        }
    }
    buf
}

/// Decode a Command from a binary blob produced by `encode`.
#[allow(unused_assignments)] // pos updated by macros even at the last read
pub fn decode(bytes: &[u8]) -> Result<Command, JournalError> {
    let mut pos = 0;

    macro_rules! read_u8 {
        () => {{
            if pos >= bytes.len() {
                return Err(JournalError::InvalidData);
            }
            let v = bytes[pos];
            pos += 1;
            v
        }};
    }
    macro_rules! read_u32 {
        () => {{
            if pos + 4 > bytes.len() {
                return Err(JournalError::InvalidData);
            }
            let v = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap());
            pos += 4;
            v
        }};
    }
    macro_rules! read_u64 {
        () => {{
            if pos + 8 > bytes.len() {
                return Err(JournalError::InvalidData);
            }
            let v = u64::from_le_bytes(bytes[pos..pos + 8].try_into().unwrap());
            pos += 8;
            v
        }};
    }

    let opcode = read_u8!();
    match opcode {
        0 => {
            let id = OrderId(read_u64!());
            let instrument_id = read_u32!();
            let side = decode_side(read_u8!())?;
            let price = read_u64!();
            let qty = read_u64!();
            let order_type = decode_order_type(read_u8!())?;
            let time_in_force = decode_tif(read_u8!())?;
            let visible_qty = match read_u8!() {
                0 => None,
                1 => Some(read_u64!()),
                _ => return Err(JournalError::InvalidData),
            };
            let stop_price = match read_u8!() {
                0 => None,
                1 => Some(read_u64!()),
                _ => return Err(JournalError::InvalidData),
            };
            let stp_group = match read_u8!() {
                0 => None,
                1 => Some(read_u32!()),
                _ => return Err(JournalError::InvalidData),
            };
            Ok(Command::NewOrder {
                id,
                instrument_id,
                side,
                price,
                qty,
                order_type,
                time_in_force,
                visible_qty,
                stop_price,
                stp_group,
            })
        }
        1 => {
            let id = OrderId(read_u64!());
            Ok(Command::CancelOrder { id })
        }
        2 => {
            let id = OrderId(read_u64!());
            let new_price = read_u64!();
            let new_qty = read_u64!();
            Ok(Command::ModifyOrder {
                id,
                new_price,
                new_qty,
            })
        }
        _ => Err(JournalError::InvalidData),
    }
}

#[inline]
fn push_option_u64(buf: &mut Vec<u8>, v: Option<u64>) {
    match v {
        None => buf.push(0),
        Some(x) => {
            buf.push(1);
            buf.extend_from_slice(&x.to_le_bytes());
        }
    }
}

#[inline]
fn push_option_u32(buf: &mut Vec<u8>, v: Option<u32>) {
    match v {
        None => buf.push(0),
        Some(x) => {
            buf.push(1);
            buf.extend_from_slice(&x.to_le_bytes());
        }
    }
}

fn decode_side(b: u8) -> Result<Side, JournalError> {
    match b {
        0 => Ok(Side::Bid),
        1 => Ok(Side::Ask),
        _ => Err(JournalError::InvalidData),
    }
}

fn decode_order_type(b: u8) -> Result<OrderType, JournalError> {
    match b {
        0 => Ok(OrderType::Limit),
        1 => Ok(OrderType::Market),
        2 => Ok(OrderType::PostOnly),
        3 => Ok(OrderType::StopLimit),
        4 => Ok(OrderType::Iceberg),
        _ => Err(JournalError::InvalidData),
    }
}

fn decode_tif(b: u8) -> Result<TimeInForce, JournalError> {
    match b {
        0 => Ok(TimeInForce::GTC),
        1 => Ok(TimeInForce::IOC),
        2 => Ok(TimeInForce::FOK),
        _ => Err(JournalError::InvalidData),
    }
}

#[cfg(test)]
mod tests {
    use super::{decode_record, encode_record};
    use matchx_types::{Command, OrderId, OrderType, Side, TimeInForce};

    #[test]
    fn framed_record_roundtrips_command_and_sequence() {
        let expected_cmd = sample_new_order();
        let bytes = encode_record(7, &expected_cmd);
        let (seq, cmd, used) = decode_record(&bytes).unwrap();
        assert_eq!(seq, 7);
        assert_eq!(super::encode(&cmd), super::encode(&expected_cmd));
        assert_eq!(used, bytes.len());
    }

    #[test]
    fn rejects_oversized_payload_len() {
        let mut data = vec![0u8; 16];
        data[0..4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(decode_record(&data).is_err());
    }

    fn sample_new_order() -> Command {
        Command::NewOrder {
            id: OrderId(11),
            instrument_id: 1,
            side: Side::Bid,
            price: 100,
            qty: 10,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            visible_qty: None,
            stop_price: None,
            stp_group: None,
        }
    }
}
