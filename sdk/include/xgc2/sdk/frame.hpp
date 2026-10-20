#pragma once

// Helpers for the byte frame convention of <xgc2/frame.h>: fill and read a frame of any size class.

#include <xgc2/frame.h>

#include <cstddef>
#include <cstdint>
#include <cstring>
#include <type_traits>

namespace xgc2 {
namespace sdk {

// Payload capacity of a frame class.
template <typename Frame>
constexpr uint32_t frameCapacity() {
    return static_cast<uint32_t>(sizeof(Frame::data));
}

// Fill `frame` for transmission. False (the frame is left zeroed) when `len` exceeds the capacity.
template <typename Frame>
bool fillFrame(Frame& frame, uint16_t peer, uint16_t kind, const void* data, size_t len,
               int64_t t_tx_ns = 0) {
    static_assert(std::is_trivially_copyable<Frame>::value, "frames are plain data");
    std::memset(&frame, 0, sizeof(frame));
    if (len > frameCapacity<Frame>()) {
        return false;
    }
    frame.peer = peer;
    frame.kind = kind;
    frame.len = static_cast<uint16_t>(len);
    frame.t_tx_ns = t_tx_ns;
    if (len > 0U) {
        std::memcpy(frame.data, data, len);
    }
    return true;
}

// The payload of a received frame as T; false when the length is not exactly sizeof(T).
template <typename T, typename Frame>
bool frameAs(const Frame& frame, T& out) {
    static_assert(std::is_trivially_copyable<T>::value, "wire payloads are plain data");
    if (frame.len != sizeof(T) || frame.len > frameCapacity<Frame>()) {
        return false;
    }
    std::memcpy(&out, frame.data, sizeof(T));
    return true;
}

}  // namespace sdk
}  // namespace xgc2
