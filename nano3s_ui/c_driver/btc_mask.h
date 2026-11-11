#ifndef BTC_MASK_H
#define BTC_MASK_H

#include <stdint.h>

#define BTC_MASK_Y_MIN 73
#define BTC_MASK_Y_MAX 167
#define BTC_MASK_X_MIN 90
#define BTC_MASK_X_MAX 153

static const uint64_t btc_mask_rows[95] = {
    0x00000ffe003ffc00ULL, /* y = 73 */
    0x00000ffe003ffc00ULL, /* y = 74 */
    0x00000ffe003ffc00ULL, /* y = 75 */
    0x00000ffe003ffc00ULL, /* y = 76 */
    0x00000ffe003ffc00ULL, /* y = 77 */
    0x00000ffe003ffc00ULL, /* y = 78 */
    0x00000ffe003ffc00ULL, /* y = 79 */
    0x00000ffe003ffc00ULL, /* y = 80 */
    0x00000ffe003ffc00ULL, /* y = 81 */
    0x00000ffe003ffc00ULL, /* y = 82 */
    0x00000fffffffffffULL, /* y = 83 */
    0x00003fffffffffffULL, /* y = 84 */
    0x0000ffffffffffffULL, /* y = 85 */
    0x0003ffffffffffffULL, /* y = 86 */
    0x000fffffffffffffULL, /* y = 87 */
    0x001fffffffffffffULL, /* y = 88 */
    0x003fffffffffffffULL, /* y = 89 */
    0x007fffffffffffffULL, /* y = 90 */
    0x00ffffffffffffffULL, /* y = 91 */
    0x00ffffffffffffffULL, /* y = 92 */
    0x01ffffffffffffffULL, /* y = 93 */
    0x01ffffffffffffffULL, /* y = 94 */
    0x03ffffffffffffffULL, /* y = 95 */
    0x03ffffc000007fffULL, /* y = 96 */
    0x03fffe0000007fffULL, /* y = 97 */
    0x07fffc0000007fffULL, /* y = 98 */
    0x07fff80000007fffULL, /* y = 99 */
    0x07fff80000007fffULL, /* y = 100 */
    0x07fff00000007fffULL, /* y = 101 */
    0x07fff00000007fffULL, /* y = 102 */
    0x07fff00000007fffULL, /* y = 103 */
    0x07fff00000007fffULL, /* y = 104 */
    0x07fff00000007fffULL, /* y = 105 */
    0x03fff00000007fffULL, /* y = 106 */
    0x03fff00000007fffULL, /* y = 107 */
    0x03fff80000007fffULL, /* y = 108 */
    0x01fffc0000007fffULL, /* y = 109 */
    0x01fffe0000007fffULL, /* y = 110 */
    0x00ffff0000007fffULL, /* y = 111 */
    0x00ffffe000007fffULL, /* y = 112 */
    0x007fffffffffffffULL, /* y = 113 */
    0x003fffffffffffffULL, /* y = 114 */
    0x001fffffffffffffULL, /* y = 115 */
    0x000fffffffffffffULL, /* y = 116 */
    0x0003ffffffffffffULL, /* y = 117 */
    0x0001ffffffffffffULL, /* y = 118 */
    0x0007ffffffffffffULL, /* y = 119 */
    0x001fffffffffffffULL, /* y = 120 */
    0x007fffffffffffffULL, /* y = 121 */
    0x00ffffffffffffffULL, /* y = 122 */
    0x01ffffffffffffffULL, /* y = 123 */
    0x03ffffffffffffffULL, /* y = 124 */
    0x07ffffffffffffffULL, /* y = 125 */
    0x0fffffc000007fffULL, /* y = 126 */
    0x0ffffc0000007fffULL, /* y = 127 */
    0x1ffff00000007fffULL, /* y = 128 */
    0x1fffe00000007fffULL, /* y = 129 */
    0x3fffe00000007fffULL, /* y = 130 */
    0x3fffc00000007fffULL, /* y = 131 */
    0x3fffc00000007fffULL, /* y = 132 */
    0x3fff800000007fffULL, /* y = 133 */
    0x7fff800000007fffULL, /* y = 134 */
    0x7fff800000007fffULL, /* y = 135 */
    0x7fff800000007fffULL, /* y = 136 */
    0x7fff800000007fffULL, /* y = 137 */
    0x7fff800000007fffULL, /* y = 138 */
    0x3fffc00000007fffULL, /* y = 139 */
    0x3fffc00000007fffULL, /* y = 140 */
    0x3fffe00000007fffULL, /* y = 141 */
    0x3ffff00000007fffULL, /* y = 142 */
    0x1ffffc0000007fffULL, /* y = 143 */
    0x1fffff0000007fffULL, /* y = 144 */
    0x1fffffffffffffffULL, /* y = 145 */
    0x0fffffffffffffffULL, /* y = 146 */
    0x0fffffffffffffffULL, /* y = 147 */
    0x07ffffffffffffffULL, /* y = 148 */
    0x03ffffffffffffffULL, /* y = 149 */
    0x03ffffffffffffffULL, /* y = 150 */
    0x01ffffffffffffffULL, /* y = 151 */
    0x00ffffffffffffffULL, /* y = 152 */
    0x003fffffffffffffULL, /* y = 153 */
    0x001fffffffffffffULL, /* y = 154 */
    0x0007ffffffffffffULL, /* y = 155 */
    0x0000ffffffffffffULL, /* y = 156 */
    0x00000fffffffffffULL, /* y = 157 */
    0x00000ffe003ffc00ULL, /* y = 158 */
    0x00000ffe003ffc00ULL, /* y = 159 */
    0x00000ffe003ffc00ULL, /* y = 160 */
    0x00000ffe003ffc00ULL, /* y = 161 */
    0x00000ffe003ffc00ULL, /* y = 162 */
    0x00000ffe003ffc00ULL, /* y = 163 */
    0x00000ffe003ffc00ULL, /* y = 164 */
    0x00000ffe003ffc00ULL, /* y = 165 */
    0x00000ffe003ffc00ULL, /* y = 166 */
    0x00000ffe003ffc00ULL, /* y = 167 */
};

static inline int is_btc_mask(int x, int y) {
    if (y < BTC_MASK_Y_MIN || y > BTC_MASK_Y_MAX || x < BTC_MASK_X_MIN || x > BTC_MASK_X_MAX) return 0;
    return (int)((btc_mask_rows[y - BTC_MASK_Y_MIN] >> (x - BTC_MASK_X_MIN)) & 1ULL);
}


static inline int is_btc_border(int x, int y) {
    if (is_btc_mask(x, y)) return 0;
    return is_btc_mask(x + 1, y) || is_btc_mask(x - 1, y) ||
           is_btc_mask(x, y + 1) || is_btc_mask(x - 1, y) ||
           is_btc_mask(x + 1, y + 1) || is_btc_mask(x - 1, y - 1) ||
           is_btc_mask(x + 1, y - 1) || is_btc_mask(x - 1, y + 1);
}

#endif /* BTC_MASK_H */
