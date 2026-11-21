#include "driver.h"
#include "lvgl.h"
#include "btc_mask.h"
#include "ui_assets.h"
#include "ui_fonts.h"
#include "ixlogo_anim.h"
#include "tinf.h"

#include <ctype.h>
#include <fcntl.h>
#include <math.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/time.h>
#include <unistd.h>

#define HOR_RES 240
#define VER_RES 240
#define FB_DEV "/dev/fb0"
#define FB_BYTES (HOR_RES * VER_RES * 2)

#define PAGE_FILE "/tmp/fb_page"
#define PIZZA_FILE "/mntapp/release/linux/app/pizza.rgb565"
#define NANO3S_LIVE_FILE "/tmp/nano3s_live.txt"
#define BLE_WIFI_STATUS_FILE "/tmp/ble_wifi_status.txt"
#define UI_THEME_TMP_FILE "/tmp/ui_theme"
#define UI_THEME_CONF_FILE "/data/userconfig/theme.conf"

#define REFRESH_INTERVAL_MS 2000
#define POLL_INTERVAL_MS 25
#define ANIM_INTERVAL_MS 40

#define RGB565(r, g, b) ((uint16_t)((((r) >> 3) << 11) | (((g) >> 2) << 5) | ((b) >> 3)))

/* Brand & Theme RGB565 Colors */
#define COL_BLACK        0x0000
#define COL_WHITE        RGB565(255, 255, 255)
#define COL_CYAN         RGB565(0, 240, 255)
#define COL_MAG          RGB565(255, 46, 151)
#define COL_CYBER_GREEN  RGB565(27, 255, 156)
#define COL_AMBER        RGB565(255, 176, 0)
#define COL_YELLOW       RGB565(255, 230, 0)
#define COL_LIME         RGB565(200, 255, 0)
#define COL_HOT_RED      RGB565(255, 46, 85)
#define COL_CYBER_DIM    RGB565(122, 148, 180)
#define COL_CYBER_TRACK  RGB565(14, 26, 44)
#define COL_CYBER_LIGHT  RGB565(214, 247, 255)
#define COL_CYBER_PILL   RGB565(10, 28, 24)
#define COL_CYBER_WARN   RGB565(35, 10, 18)

#define COL_MIN_ROYAL    RGB565(0, 102, 204)
#define COL_MIN_SKY      RGB565(2, 132, 199)
#define COL_MIN_SKY_BRT  RGB565(56, 189, 248)
#define COL_MIN_MINT     RGB565(16, 185, 129)
#define COL_MIN_MINT_BRT RGB565(52, 211, 153)
#define COL_MIN_BLUE     RGB565(59, 130, 246)
#define COL_MIN_WARM     RGB565(217, 119, 6)
#define COL_MIN_HOT      RGB565(220, 38, 38)
#define COL_MIN_INK      RGB565(45, 62, 80)
#define COL_MIN_SUB      RGB565(78, 95, 118)

#define COL_PARA_DIM     RGB565(165, 165, 165)
#define COL_PARA_COOL    RGB565(130, 130, 130)
#define COL_PARA_DARK    RGB565(64, 64, 64)
#define COL_PARA_RED     RGB565(255, 51, 51)

/* Legacy LVGL colors for scr_waiting and scr_wifi_setup */
#define C_BG     lv_color_make(0x0e, 0x0f, 0x12)
#define C_TEXT_1 lv_color_make(0xe7, 0xe8, 0xec)
#define C_TEXT_2 lv_color_make(0xb5, 0xb7, 0xbf)
#define C_TEXT_3 lv_color_make(0x99, 0x9b, 0xa4)
#define C_GREEN  lv_color_make(0x39, 0xd3, 0x53)
#define C_YELLOW lv_color_make(0xff, 0xff, 0x00)
#define C_RED    lv_color_make(0xff, 0x55, 0x55)

typedef enum {
    THEME_CYBERPUNK = 0,
    THEME_MINIMAL = 1,
    THEME_PARASITE = 2
} ui_theme_t;

/* ── Display flush ────────────────────────────────────────────────────── */

static lv_color_t draw_buf1[HOR_RES * 10];
static uint16_t ui_fb[HOR_RES * VER_RES];

static void fb_flush_cb(lv_disp_drv_t *disp_drv, const lv_area_t *area, lv_color_t *color_p) {
    int fd = open(FB_DEV, O_WRONLY);
    if (fd < 0) {
        lv_disp_flush_ready(disp_drv);
        return;
    }

    int32_t w = area->x2 - area->x1 + 1;
    size_t row_bytes = (size_t)w * sizeof(lv_color_t);

    for (int32_t y = area->y1; y <= area->y2; y++) {
        off_t offset = ((off_t)y * HOR_RES + area->x1) * (off_t)sizeof(lv_color_t);
        if (lseek(fd, offset, SEEK_SET) == (off_t)-1) {
            break;
        }
        const uint8_t *src = (const uint8_t *)color_p + (size_t)(y - area->y1) * row_bytes;
        size_t written = 0;
        while (written < row_bytes) {
            ssize_t n = write(fd, src + written, row_bytes - written);
            if (n <= 0) {
                break;
            }
            written += (size_t)n;
        }
    }

    close(fd);
    lv_disp_flush_ready(disp_drv);
}

static void blit_ui_fb(const uint16_t *fb) {
    int fd = open(FB_DEV, O_WRONLY);
    if (fd < 0) {
        return;
    }
    lseek(fd, 0, SEEK_SET);
    size_t written = 0;
    size_t total = HOR_RES * VER_RES * sizeof(uint16_t);
    const uint8_t *ptr = (const uint8_t *)fb;
    while (written < total) {
        ssize_t wn = write(fd, ptr + written, total - written);
        if (wn <= 0) {
            break;
        }
        written += (size_t)wn;
    }
    close(fd);
}

static uint32_t now_ms(void) {
    struct timeval tv;
    gettimeofday(&tv, NULL);
    return (uint32_t)((uint64_t)tv.tv_sec * 1000u + (uint64_t)tv.tv_usec / 1000u);
}

/* ── key=value telemetry parsing ──────────────────────────────────────── */

#define KV_MAX_ENTRIES 32
#define KV_BUF_SIZE 4096

typedef struct {
    const char *key;
    const char *val;
} kv_entry_t;

static char kv_buf[KV_BUF_SIZE];
static kv_entry_t kv_entries[KV_MAX_ENTRIES];
static int kv_count;

static int read_kv_file(const char *path) {
    kv_count = 0;
    FILE *f = fopen(path, "r");
    if (!f) {
        return 0;
    }
    size_t n = fread(kv_buf, 1, sizeof(kv_buf) - 1, f);
    fclose(f);
    kv_buf[n] = '\0';

    char *line = kv_buf;
    while (line < kv_buf + n && kv_count < KV_MAX_ENTRIES) {
        char *nl = strchr(line, '\n');
        if (nl) {
            *nl = '\0';
        }
        char *eq = strchr(line, '=');
        if (eq) {
            *eq = '\0';
            kv_entries[kv_count].key = line;
            kv_entries[kv_count].val = eq + 1;
            kv_count++;
        }
        if (!nl) {
            break;
        }
        line = nl + 1;
    }
    return 1;
}

static const char *kv_str(const char *key) {
    for (int i = 0; i < kv_count; i++) {
        if (strcmp(kv_entries[i].key, key) == 0) {
            return kv_entries[i].val;
        }
    }
    return NULL;
}

static int kv_f64(const char *key, double *out) {
    const char *s = kv_str(key);
    if (!s || *s == '\0') return 0;
    char *end;
    double v = strtod(s, &end);
    if (end == s) return 0;
    *out = v;
    return 1;
}

static int kv_u32(const char *key, uint32_t *out) {
    const char *s = kv_str(key);
    if (!s || *s == '\0') return 0;
    char *end;
    unsigned long v = strtoul(s, &end, 10);
    if (end == s) return 0;
    *out = (uint32_t)v;
    return 1;
}

static int kv_bool(const char *key) {
    const char *s = kv_str(key);
    return s && strcmp(s, "1") == 0;
}

static ui_theme_t parse_theme_str(const char *s) {
    if (!s) return THEME_CYBERPUNK;
    while (*s && isspace((unsigned char)*s)) s++;
    if (strncmp(s, "minimal", 7) == 0 || strncmp(s, "apple", 5) == 0) return THEME_MINIMAL;
    if (strncmp(s, "parasite", 8) == 0) return THEME_PARASITE;
    return THEME_CYBERPUNK;
}

static ui_theme_t read_ui_theme(void) {
    char buf[64];
    FILE *f = fopen(UI_THEME_TMP_FILE, "r");
    if (f) {
        if (fgets(buf, sizeof(buf), f)) {
            fclose(f);
            return parse_theme_str(buf);
        }
        fclose(f);
    }
    f = fopen(UI_THEME_CONF_FILE, "r");
    if (f) {
        while (fgets(buf, sizeof(buf), f)) {
            char *eq = strchr(buf, '=');
            if (eq) {
                if (strncmp(buf, "theme", 5) == 0) {
                    fclose(f);
                    return parse_theme_str(eq + 1);
                }
            } else {
                fclose(f);
                return parse_theme_str(buf);
            }
        }
        fclose(f);
    }
    return THEME_CYBERPUNK;
}

/* ── Network lookups ──────────────────────────────────────────────────── */

static int wlan_ip(char *out, size_t out_size) {
    FILE *p = popen("ifconfig wlan0 2>/dev/null", "r");
    if (!p) return 0;
    char buf[2048];
    size_t n = fread(buf, 1, sizeof(buf) - 1, p);
    pclose(p);
    buf[n] = '\0';

    const char *needle = "inet addr:";
    char *start = strstr(buf, needle);
    if (!start) return 0;
    start += strlen(needle);
    char *end = start;
    while (*end && !isspace((unsigned char)*end)) {
        end++;
    }
    size_t len = (size_t)(end - start);
    if (len == 0 || len >= out_size) return 0;
    memcpy(out, start, len);
    out[len] = '\0';
    return 1;
}

static int wlan_rssi_dbm(int *out_dbm) {
    FILE *f = fopen("/proc/net/wireless", "r");
    if (!f) return 0;
    char line[256];
    int ok = 0;
    while (fgets(line, sizeof(line), f)) {
        char *p = strstr(line, "wlan0:");
        if (p) {
            int status = 0;
            float link = 0, level = 0;
            if (sscanf(p + 6, "%d %f %f", &status, &link, &level) == 3) {
                *out_dbm = (int)roundf(level);
                ok = 1;
            }
            break;
        }
    }
    fclose(f);
    return ok;
}

static void silence_buzzer(void) {
    int fd = open("/dev/input/event1", O_WRONLY);
    if (fd < 0) return;
    uint8_t event[24];
    memset(event, 0, sizeof(event));
    event[16] = 0x12; /* EV_SND */
    event[18] = 0x02; /* SND_TONE */
    ssize_t ignore = write(fd, event, sizeof(event));
    (void)ignore;
    event[16] = 0x00; /* EV_SYN */
    event[18] = 0x00;
    ignore = write(fd, event, sizeof(event));
    (void)ignore;
    close(fd);
}

/* ── Fast Polar Annulus LUT & Direct RGB565 Primitives ────────────────── */

#define RING_LUT_MAX 22000

typedef struct {
    uint16_t idx;
    float r;
    float deg; /* [0, 360) clockwise from 3 o'clock (0=+X, 90=+Y) */
} ring_lut_px_t;

static ring_lut_px_t ring_lut[RING_LUT_MAX];
static int ring_lut_len = 0;

static void init_ring_lut(void) {
    ring_lut_len = 0;
    for (int y = 0; y < VER_RES; y++) {
        float dy = (float)y - 119.5f;
        for (int x = 0; x < HOR_RES; x++) {
            float dx = (float)x - 119.5f;
            float r = sqrtf(dx * dx + dy * dy);
            if (r >= 84.0f && r <= 116.5f && ring_lut_len < RING_LUT_MAX) {
                float deg = atan2f(dy, dx) * 57.2957795f;
                if (deg < 0.0f) deg += 360.0f;
                ring_lut[ring_lut_len].idx = (uint16_t)(y * HOR_RES + x);
                ring_lut[ring_lut_len].r = r;
                ring_lut[ring_lut_len].deg = deg;
                ring_lut_len++;
            }
        }
    }
}

static inline uint16_t blend565(uint16_t bg, uint16_t fg, uint8_t alpha) {
    if (alpha == 0) return bg;
    if (alpha == 255) return fg;
    uint32_t a = alpha;
    uint32_t inv = 255u - a;
    uint32_t r = (((bg >> 11) & 0x1F) * inv + ((fg >> 11) & 0x1F) * a + 127u) / 255u;
    uint32_t g = (((bg >> 5) & 0x3F) * inv + ((fg >> 5) & 0x3F) * a + 127u) / 255u;
    uint32_t b = ((bg & 0x1F) * inv + (fg & 0x1F) * a + 127u) / 255u;
    return (uint16_t)((r << 11) | (g << 5) | b);
}

static inline uint16_t lerp565(uint16_t c1, uint16_t c2, float t) {
    if (t <= 0.0f) return c1;
    if (t >= 1.0f) return c2;
    return blend565(c1, c2, (uint8_t)(t * 255.0f));
}

static inline float wrap360(float a) {
    a = fmodf(a, 360.0f);
    if (a < 0.0f) a += 360.0f;
    return a;
}

static void ui_draw_wedge_arc(float r_mid, float start_deg, float sweep_deg, float width, uint16_t col, uint8_t opa) {
    if (sweep_deg <= 0.05f || width <= 0.1f) return;
    float half_w = width * 0.5f;
    float s_norm = wrap360(start_deg);
    float end_deg = wrap360(s_norm + sweep_deg);
    int full_circle = (sweep_deg >= 359.9f);

    for (int i = 0; i < ring_lut_len; i++) {
        float dr = fabsf(ring_lut[i].r - r_mid);
        if (dr > half_w + 0.75f) continue;

        float cov_r = (dr <= half_w - 0.5f) ? 1.0f : (half_w + 0.75f - dr) / 1.25f;
        if (cov_r <= 0.0f) continue;

        float cov_a = 1.0f;
        if (!full_circle) {
            float rel = ring_lut[i].deg - s_norm;
            if (rel < 0.0f) rel += 360.0f;
            if (rel > sweep_deg) {
                /* Check angular AA near start or end boundary */
                float d_start = 360.0f - rel;
                float d_end = rel - sweep_deg;
                float d_ang = (d_start < d_end) ? d_start : d_end;
                float px_arc = d_ang * 0.0174532925f * ring_lut[i].r;
                if (px_arc > 0.75f) continue;
                cov_a = (0.75f - px_arc) / 1.5f;
            } else {
                float d_edge = (rel < sweep_deg - rel) ? rel : (sweep_deg - rel);
                float px_arc = d_edge * 0.0174532925f * ring_lut[i].r;
                if (px_arc < 0.75f) {
                    cov_a = (px_arc + 0.75f) / 1.5f;
                }
            }
        }
        float a_f = cov_r * cov_a * (float)opa;
        if (a_f > 1.0f) {
            uint16_t idx = ring_lut[i].idx;
            ui_fb[idx] = blend565(ui_fb[idx], col, (uint8_t)a_f);
        }
    }
}

static void ui_draw_smooth_arc(float r_mid, float start_deg, float sweep_deg, float width,
                               uint16_t col_start, uint16_t col_end, uint8_t opa) {
    if (sweep_deg <= 0.05f || width <= 0.1f) return;
    float half_w = width * 0.5f;
    float s_norm = wrap360(start_deg);
    float s_rad = s_norm * 0.0174532925f;
    float e_rad = wrap360(s_norm + sweep_deg) * 0.0174532925f;
    float cx_s = 119.5f + r_mid * cosf(s_rad);
    float cy_s = 119.5f + r_mid * sinf(s_rad);
    float cx_e = 119.5f + r_mid * cosf(e_rad);
    float cy_e = 119.5f + r_mid * sinf(e_rad);
    int full_circle = (sweep_deg >= 359.9f);

    for (int i = 0; i < ring_lut_len; i++) {
        float dr = fabsf(ring_lut[i].r - r_mid);
        if (dr > half_w + 0.75f) continue;

        uint16_t idx = ring_lut[i].idx;
        int x = idx % HOR_RES;
        int y = idx / HOR_RES;
        float rel = ring_lut[i].deg - s_norm;
        if (rel < 0.0f) rel += 360.0f;

        float dist = 999.0f;
        float t = 0.0f;
        if (full_circle || rel <= sweep_deg) {
            dist = dr;
            t = rel / sweep_deg;
        } else {
            float dx1 = (float)x - cx_s, dy1 = (float)y - cy_s;
            float max_d = half_w + 0.75f;
            float max_d_sq = max_d * max_d;
            float d1_sq = dx1 * dx1 + dy1 * dy1;
            float dx2 = (float)x - cx_e, dy2 = (float)y - cy_e;
            float d2_sq = dx2 * dx2 + dy2 * dy2;
            if (d1_sq > max_d_sq && d2_sq > max_d_sq) continue;
            if (d1_sq < d2_sq) {
                dist = sqrtf(d1_sq);
                t = 0.0f;
            } else {
                dist = sqrtf(d2_sq);
                t = 1.0f;
            }
        }
        if (dist > half_w + 0.75f) continue;
        float cov = (dist <= half_w - 0.55f) ? 1.0f : (half_w + 0.75f - dist) / 1.3f;
        if (cov <= 0.0f) continue;
        uint16_t col = (col_start == col_end) ? col_start : lerp565(col_start, col_end, t);
        uint8_t a = (uint8_t)(cov * (float)opa);
        if (a > 0) ui_fb[idx] = blend565(ui_fb[idx], col, a);
    }
}

static void ui_draw_radial_ticks(int n_ticks, float start_deg, float span_deg, float r_in_minor, float r_in_major,
                                 float r_out, int major_mod, int active_limit, uint16_t col_on, uint16_t col_alert,
                                 int alert_from, uint16_t col_off, float half_w_on, float half_w_off) {
    if (n_ticks <= 0) return;
    int full_circle = (span_deg >= 359.5f);
    float step = (n_ticks > 1 && !full_circle) ? (span_deg / (float)(n_ticks - 1)) : (span_deg / (float)n_ticks);
    float s_norm = wrap360(start_deg);
    float r_min = (r_in_major < r_in_minor) ? r_in_major : r_in_minor;

    for (int k = 0; k < ring_lut_len; k++) {
        float r = ring_lut[k].r;
        if (r < r_min - 0.6f || r > r_out + 0.6f) continue;

        int i;
        if (n_ticks == 1) {
            i = 0;
        } else {
            float rel = ring_lut[k].deg - s_norm;
            if (rel < 0.0f) rel += 360.0f;
            if (full_circle) {
                i = ((int)floorf((rel + 0.5f * step) / step)) % n_ticks;
                if (i < 0) i += n_ticks;
            } else if (rel > span_deg + 4.5f) {
                float d0 = 360.0f - rel;
                if (d0 <= 4.5f && d0 < (rel - span_deg)) i = 0;
                else continue;
            } else {
                i = (int)floorf((rel + 0.5f * step) / step);
                if (i < 0) i = 0;
                if (i >= n_ticks) i = n_ticks - 1;
            }
        }

        int is_on = (i < active_limit);
        int is_maj = (major_mod > 0) && (i % major_mod == 0);
        float r_in = is_maj ? r_in_major : r_in_minor;
        if (r < r_in - 0.6f) continue;

        float ang_deg = wrap360(s_norm + (float)i * step);
        float da = fabsf(ring_lut[k].deg - ang_deg);
        if (da > 180.0f) da = 360.0f - da;
        if (da > 4.5f) continue;

        float hw = is_on ? half_w_on : half_w_off;
        float perp = r * sinf(da * 0.0174532925f);
        if (perp > hw + 0.65f) continue;

        uint16_t col = is_on ? ((alert_from >= 0 && i >= alert_from) ? col_alert : col_on) : col_off;
        float cov_w = (perp <= hw - 0.45f) ? 1.0f : (hw + 0.65f - perp) / 1.1f;
        float cov_r = 1.0f;
        if (r < r_in + 0.45f) cov_r = (r - (r_in - 0.6f)) / 1.05f;
        else if (r > r_out - 0.45f) cov_r = ((r_out + 0.6f) - r) / 1.05f;
        if (cov_r < 0.0f) cov_r = 0.0f;
        if (cov_r > 1.0f) cov_r = 1.0f;
        uint8_t a = (uint8_t)(cov_w * cov_r * 255.0f);
        if (a > 0) {
            uint16_t idx = ring_lut[k].idx;
            ui_fb[idx] = blend565(ui_fb[idx], col, a);
        }
    }
}

static void ui_fill_circle(float cx, float cy, float radius, uint16_t col, uint8_t opa) {
    int x0 = (int)floorf(cx - radius - 1.5f);
    int x1 = (int)ceilf(cx + radius + 1.5f);
    int y0 = (int)floorf(cy - radius - 1.5f);
    int y1 = (int)ceilf(cy + radius + 1.5f);
    if (x0 < 0) x0 = 0;
    if (y0 < 0) y0 = 0;
    if (x1 >= HOR_RES) x1 = HOR_RES - 1;
    if (y1 >= VER_RES) y1 = VER_RES - 1;
    for (int y = y0; y <= y1; y++) {
        for (int x = x0; x <= x1; x++) {
            float d = sqrtf(((float)x - cx) * ((float)x - cx) + ((float)y - cy) * ((float)y - cy));
            if (d <= radius + 0.7f) {
                float cov = (d <= radius - 0.5f) ? 1.0f : (radius + 0.7f - d) / 1.2f;
                uint8_t a = (uint8_t)(cov * (float)opa);
                if (a > 0) {
                    int idx = y * HOR_RES + x;
                    ui_fb[idx] = blend565(ui_fb[idx], col, a);
                }
            }
        }
    }
}

static void ui_fill_rect(int x, int y, int w, int h, uint16_t col) {
    int x0 = (x < 0) ? 0 : x;
    int y0 = (y < 0) ? 0 : y;
    int x1 = (x + w > HOR_RES) ? HOR_RES : (x + w);
    int y1 = (y + h > VER_RES) ? VER_RES : (y + h);
    for (int yy = y0; yy < y1; yy++) {
        for (int xx = x0; xx < x1; xx++) {
            ui_fb[yy * HOR_RES + xx] = col;
        }
    }
}

static void ui_draw_rect_outline(int x, int y, int w, int h, int stroke, uint16_t col) {
    ui_fill_rect(x, y, w, stroke, col);
    ui_fill_rect(x, y + h - stroke, w, stroke, col);
    ui_fill_rect(x, y, stroke, h, col);
    ui_fill_rect(x + w - stroke, y, stroke, h, col);
}

static void ui_draw_chamfer_pill(int x, int y, int w, int h, uint16_t fill, uint16_t border) {
    ui_fill_rect(x + 2, y + 1, w - 4, h - 2, fill);
    ui_fill_rect(x + 1, y + 2, w - 2, h - 4, fill);
    ui_fill_rect(x, y, w - 3, 1, border);
    ui_fill_rect(x + 3, y + h - 1, w - 3, 1, border);
    ui_fill_rect(x, y, 1, h - 3, border);
    ui_fill_rect(x + w - 1, y + 3, 1, h - 3, border);
    for (int i = 0; i <= 3; i++) {
        int px1 = x + w - 4 + i, py1 = y + i;
        if ((unsigned)px1 < HOR_RES && (unsigned)py1 < VER_RES) ui_fb[py1 * HOR_RES + px1] = border;
        int px2 = x + i, py2 = y + h - 4 + i;
        if ((unsigned)px2 < HOR_RES && (unsigned)py2 < VER_RES) ui_fb[py2 * HOR_RES + px2] = border;
    }
}

/* ── Anti-Aliased UTF-8 Bitmap Font Rendering ─────────────────────────── */

static const ui_glyph_t *find_glyph(const ui_font_t *font, uint16_t cp) {
    for (uint16_t i = 0; i < font->glyph_count; i++) {
        if (font->glyphs[i].codepoint == cp) {
            return &font->glyphs[i];
        }
    }
    return &font->glyphs[0];
}

static uint16_t next_utf8_cp(const char **p) {
    const uint8_t *s = (const uint8_t *)(*p);
    uint8_t c0 = s[0];
    if (c0 < 0x80) {
        (*p)++;
        return c0;
    }
    if ((c0 & 0xE0) == 0xC0 && s[1]) {
        uint16_t cp = (uint16_t)(((c0 & 0x1F) << 6) | (s[1] & 0x3F));
        (*p) += 2;
        return cp;
    }
    if ((c0 & 0xF0) == 0xE0 && s[1] && s[2]) {
        uint16_t cp = (uint16_t)(((c0 & 0x0F) << 12) | ((s[1] & 0x3F) << 6) | (s[2] & 0x3F));
        (*p) += 3;
        return cp;
    }
    (*p)++;
    return '?';
}

static int measure_text_px(const ui_font_t *font, const char *txt) {
    int total = 0;
    const char *p = txt;
    while (*p) {
        uint16_t cp = next_utf8_cp(&p);
        total += find_glyph(font, cp)->advance;
    }
    return total;
}

static void draw_text_xy(const ui_font_t *font, int x, int y, const char *txt, uint16_t col) {
    int pen_x = x;
    const char *p = txt;
    while (*p) {
        uint16_t cp = next_utf8_cp(&p);
        const ui_glyph_t *g = find_glyph(font, cp);
        if (g->width > 0 && g->height > 0) {
            const uint8_t *src = font->bitmap + g->data_offset;
            int gx = pen_x + g->x_off;
            int gy = y + g->y_off;
            for (int r = 0; r < g->height; r++) {
                int yy = gy + r;
                if ((unsigned)yy >= VER_RES) continue;
                for (int c = 0; c < g->width; c++) {
                    int xx = gx + c;
                    if ((unsigned)xx >= HOR_RES) continue;
                    uint8_t alpha = src[r * g->width + c];
                    if (alpha > 4) {
                        int idx = yy * HOR_RES + xx;
                        ui_fb[idx] = blend565(ui_fb[idx], col, alpha);
                    }
                }
            }
        }
        pen_x += g->advance;
    }
}

static void draw_text_centered(const ui_font_t *font, int cx, int y, const char *txt, uint16_t col) {
    int w = measure_text_px(font, txt);
    draw_text_xy(font, cx - w / 2, y, txt, col);
}

static const ui_font_t *fit_font_or_fallback(const ui_font_t *primary, const ui_font_t *fallback, int max_w, const char *txt) {
    if (measure_text_px(primary, txt) <= max_w) return primary;
    return fallback;
}

/* ── Thermal Color Mapping per Theme ──────────────────────────────────── */

static uint16_t cyber_temp_color(double t) {
    if (t < 65.0) return COL_CYBER_GREEN;
    if (t < 75.0) return COL_CYAN;
    if (t < 83.0) return COL_YELLOW;
    if (t < 85.0) return COL_MAG;
    return COL_HOT_RED;
}

static uint16_t minimal_temp_color(double t) {
    if (t < 66.0) return COL_MIN_MINT;
    if (t < 76.0) return COL_MIN_ROYAL;
    if (t < 83.5) return COL_MIN_WARM;
    return COL_MIN_HOT;
}

static uint16_t parasite_temp_color(double t) {
    if (t >= 84.0) return COL_PARA_RED;
    if (t < 65.0) return COL_PARA_COOL;
    return COL_WHITE;
}

static void uppercase_str(char *dst, size_t dst_sz, const char *src) {
    size_t i = 0;
    while (src[i] && i + 1 < dst_sz) {
        dst[i] = (char)toupper((unsigned char)src[i]);
        i++;
    }
    dst[i] = '\0';
}

/* ── Theme-Synced Screens (Direct RGB565 High-Precision Renderer) ─────── */

static void refresh_splash_themed(ui_theme_t theme, uint32_t ms) {
    float t = (float)(ms % 120000u) * 0.001f;
    if (theme == THEME_MINIMAL) {
        memcpy(ui_fb, bg_splash_minimal, sizeof(ui_fb));
        float rot = fmodf(t * 36.0f, 360.0f);
        ui_draw_smooth_arc(107.0f, rot, 130.0f, 6.0f, COL_MIN_ROYAL, COL_MIN_SKY_BRT, 255);
        ui_draw_smooth_arc(107.0f, rot + 180.0f, 120.0f, 6.0f, COL_MIN_MINT, COL_MIN_MINT_BRT, 255);
        /* Subtle orbiting specular glass beads on inner radius */
        float orb = fmodf(-t * 28.0f, 360.0f) * 0.0174532925f;
        ui_fill_circle(119.5f + 95.0f * cosf(orb), 119.5f + 95.0f * sinf(orb), 2.8f, COL_WHITE, 210);
        ui_fill_circle(119.5f - 95.0f * cosf(orb), 119.5f - 95.0f * sinf(orb), 2.8f, COL_MIN_SKY_BRT, 190);
    } else if (theme == THEME_PARASITE) {
        memcpy(ui_fb, bg_splash_parasite, sizeof(ui_fb));
        float rot_ticks = fmodf(t * 20.0f, 360.0f);
        float rot_scan = fmodf(-t * 52.0f, 360.0f);
        ui_draw_radial_ticks(36, rot_ticks, 360.0f, 106.0f, 102.5f, 111.5f, 3, 36, COL_WHITE, COL_WHITE, -1, COL_PARA_DARK, 0.9f, 0.6f);
        ui_draw_wedge_arc(98.5f, rot_scan, 52.0f, 2.6f, COL_WHITE, 255);
        ui_draw_wedge_arc(98.5f, rot_scan + 180.0f, 52.0f, 2.6f, COL_PARA_RED, 255);
    } else {
        memcpy(ui_fb, bg_splash_cyber, sizeof(ui_fb));
        float rot1 = fmodf(t * 45.0f, 360.0f);
        float rot2 = fmodf(-t * 30.0f, 360.0f);
        ui_draw_wedge_arc(109.0f, rot1 + 20.0f, 52.0f, 5.5f, COL_CYAN, 255);
        ui_draw_wedge_arc(109.0f, rot1 + 108.0f, 52.0f, 5.5f, COL_CYAN, 255);
        ui_draw_wedge_arc(109.0f, rot1 + 200.0f, 52.0f, 5.5f, COL_CYAN, 255);
        ui_draw_wedge_arc(109.0f, rot1 + 288.0f, 52.0f, 5.5f, COL_CYAN, 255);
        ui_draw_wedge_arc(109.0f, rot1 + 78.0f, 24.0f, 5.5f, COL_MAG, 255);
        ui_draw_wedge_arc(109.0f, rot1 + 258.0f, 24.0f, 5.5f, COL_MAG, 255);
        /* Counter-rotating inner targeting ticks */
        ui_draw_radial_ticks(4, rot2, 360.0f, 94.0f, 94.0f, 102.5f, 1, 4, COL_CYAN, COL_CYAN, -1, COL_CYAN, 1.0f, 1.0f);
    }
    blit_ui_fb(ui_fb);
}

static void refresh_parasite_logo(uint32_t ms) {
    float t = (float)(ms % 120000u) * 0.001f;
    memcpy(ui_fb, bg_parasite_logo, sizeof(ui_fb));
    float rot_ticks = fmodf(t * 20.0f, 360.0f);
    float rot_scan = fmodf(-t * 48.0f, 360.0f);
    ui_draw_radial_ticks(36, rot_ticks, 360.0f, 106.0f, 102.5f, 111.5f, 3, 36, COL_WHITE, COL_WHITE, -1, COL_PARA_DARK, 0.9f, 0.6f);
    ui_draw_wedge_arc(99.0f, rot_scan, 68.0f, 2.8f, COL_PARA_RED, 255);
    ui_draw_wedge_arc(99.0f, rot_scan + 180.0f, 68.0f, 2.8f, COL_WHITE, 255);
    blit_ui_fb(ui_fb);
}

static void refresh_live_themed(ui_theme_t theme) {
    double ghs = 0.0, temp_avg = 0.0, temp_max = 0.0, power_w = 0.0;
    uint32_t asics = 12;
    int have_ghs = kv_f64("hashrate_ghs", &ghs);
    int have_avg = kv_f64("temp_avg", &temp_avg);
    int have_max = kv_f64("temp_max", &temp_max);
    int have_pwr = kv_f64("power_w", &power_w);
    kv_u32("asics_total", &asics);
    int paused = kv_bool("paused");
    int ipc_ok = kv_bool("ipc_connected");
    const char *shares_str = kv_str("shares_found");
    const char *mode_raw = kv_str("autotune_mode");
    char mode_up[32] = "MINING";
    if (mode_raw && *mode_raw) uppercase_str(mode_up, sizeof(mode_up), mode_raw);

    double ths = have_ghs ? (ghs / 1000.0) : 0.0;
    if (ths < 0.0) ths = 0.0;
    double temp_show = have_avg ? temp_avg : (have_max ? temp_max : 0.0);
    double hr_frac = ths / 7.0; if (hr_frac > 1.0) hr_frac = 1.0;
    double t_frac = temp_show / 90.0; if (t_frac > 1.0) t_frac = 1.0; if (t_frac < 0.0) t_frac = 0.0;
    double p_frac = power_w / 140.0; if (p_frac > 1.0) p_frac = 1.0; if (p_frac < 0.0) p_frac = 0.0;

    char hr_buf[24], temp_buf[24], pwr_buf[24], foot1[48], foot2[36], pill_buf[48];
    if (have_ghs) snprintf(hr_buf, sizeof(hr_buf), "%.2f", ths); else snprintf(hr_buf, sizeof(hr_buf), "--");
    if (have_avg || have_max) snprintf(temp_buf, sizeof(temp_buf), "%.1f\xc2\xb0""C", temp_show); else snprintf(temp_buf, sizeof(temp_buf), "--");
    if (have_pwr) snprintf(pwr_buf, sizeof(pwr_buf), "%.0f W", power_w); else snprintf(pwr_buf, sizeof(pwr_buf), "--");

    double eff = (have_pwr && ths > 0.05) ? (power_w / ths) : 0.0;

    if (theme == THEME_MINIMAL) {
        memcpy(ui_fb, bg_live_minimal, sizeof(ui_fb));
        ui_draw_smooth_arc(104.0f, 145.0f, 250.0f * (float)hr_frac, 7.5f, COL_MIN_ROYAL, COL_MIN_SKY, 255);
        ui_draw_smooth_arc(91.0f, 115.0f, 85.0f * (float)t_frac, 5.5f, COL_MIN_MINT, COL_MIN_MINT, 255);
        ui_draw_smooth_arc(91.0f, 425.0f - 85.0f * (float)p_frac, 85.0f * (float)p_frac, 5.5f, COL_MIN_BLUE, COL_MIN_BLUE, 255);

        uint16_t dot_c = (!ipc_ok) ? COL_MIN_HOT : (paused ? COL_MIN_WARM : COL_MIN_MINT);
        ui_fill_circle(73.0f, 55.0f, 3.0f, dot_c, 255);
        if (!ipc_ok) snprintf(pill_buf, sizeof(pill_buf), "IPC Offline");
        else if (paused) snprintf(pill_buf, sizeof(pill_buf), "Paused");
        else snprintf(pill_buf, sizeof(pill_buf), "Mining \xe2\x80\xa2 %s", mode_raw ? mode_raw : "Auto");
        draw_text_xy(&font_segoe_8, 81, 49, pill_buf, COL_MIN_INK);

        draw_text_centered(&font_bahn_33, 120, 78, hr_buf, COL_MIN_ROYAL);
        draw_text_centered(&font_bahn_13, 86, 150, temp_buf, COL_MIN_INK);
        draw_text_centered(&font_bahn_13, 154, 150, pwr_buf, COL_MIN_ROYAL);

        if (eff > 0.0) snprintf(foot1, sizeof(foot1), "%s Shares \xe2\x80\xa2 %.1f J/TH", shares_str ? shares_str : "0", eff);
        else snprintf(foot1, sizeof(foot1), "%s Shares", shares_str ? shares_str : "0");
        draw_text_centered(&font_segoe_8, 120, 173, foot1, COL_MIN_SUB);
        snprintf(foot2, sizeof(foot2), "%u/12 ASICs OK", asics);
        draw_text_centered(&font_segoe_8, 120, 186, foot2, COL_MIN_SUB);
    } else if (theme == THEME_PARASITE) {
        memcpy(ui_fb, bg_live_parasite, sizeof(ui_fb));
        int alert = (!paused && (temp_avg >= 85.0 || temp_max >= 90.0));
        int active_ticks = (int)round(hr_frac * 36.0);
        ui_draw_radial_ticks(36, 135.0f, 270.0f, 101.0f, 98.0f, 109.0f, 6, active_ticks,
                             COL_WHITE, COL_PARA_RED, alert ? 28 : 99, COL_PARA_DARK, 1.4f, 0.7f);

        ui_fill_rect(57, 48, 126, 16, alert ? COL_PARA_RED : COL_WHITE);
        if (!ipc_ok) snprintf(pill_buf, sizeof(pill_buf), "NO IPC CONNECTION");
        else if (paused) snprintf(pill_buf, sizeof(pill_buf), "MINER PAUSED");
        else if (alert) snprintf(pill_buf, sizeof(pill_buf), "THERMAL WARN // MAX");
        else snprintf(pill_buf, sizeof(pill_buf), "MINING // %s", mode_up);
        draw_text_centered(&font_courier_8, 120, 51, pill_buf, COL_BLACK);

        draw_text_centered(&font_courier_31, 120, 76, hr_buf, COL_WHITE);
        draw_text_centered(&font_courier_8, 84, 135, "TEMP", alert ? COL_PARA_RED : COL_PARA_DIM);
        draw_text_centered(&font_courier_8, 156, 135, "POWER", COL_PARA_DIM);
        if (have_avg || have_max) snprintf(temp_buf, sizeof(temp_buf), "%.1fC%s", temp_show, alert ? "!" : "");
        draw_text_centered(&font_courier_13, 84, 145, temp_buf, alert ? COL_PARA_RED : COL_WHITE);
        if (have_pwr) snprintf(pwr_buf, sizeof(pwr_buf), "%.0fW", power_w);
        draw_text_centered(&font_courier_13, 156, 145, pwr_buf, COL_WHITE);

        snprintf(foot1, sizeof(foot1), "SHARES: %s [OK]", shares_str ? shares_str : "0");
        draw_text_centered(&font_courier_8, 120, 173, foot1, COL_WHITE);
        if (eff > 0.0) snprintf(foot2, sizeof(foot2), "%.1f J/TH", eff); else snprintf(foot2, sizeof(foot2), "%u/12 ASICS", asics);
        draw_text_centered(&font_courier_8, 120, 185, foot2, COL_PARA_DIM);
    } else {
        /* THEME_CYBERPUNK */
        memcpy(ui_fb, bg_live_cyber, sizeof(ui_fb));
        int n_segs = 24;
        float start_a = 150.0f, seg_a = (240.0f - 23.0f * 2.2f) / 24.0f;
        int filled = (int)round(hr_frac * n_segs);
        for (int i = 0; i < n_segs; i++) {
            float a1 = start_a + (float)i * (seg_a + 2.2f);
            uint16_t sc = (i < filled) ? ((i >= 19) ? COL_MAG : COL_CYAN) : COL_CYBER_TRACK;
            ui_draw_wedge_arc(107.0f, a1, seg_a, 6.5f, sc, 255);
        }
        uint16_t t_col = cyber_temp_color(temp_max > 0.0 ? temp_max : temp_show);
        ui_draw_wedge_arc(95.0f, 115.0f, 90.0f, 4.5f, COL_CYBER_TRACK, 255);
        ui_draw_wedge_arc(95.0f, 115.0f, 90.0f * (float)t_frac, 4.5f, t_col, 255);
        ui_draw_wedge_arc(95.0f, 335.0f, 90.0f, 4.5f, COL_CYBER_TRACK, 255);
        ui_draw_wedge_arc(95.0f, 425.0f - 90.0f * (float)p_frac, 90.0f * (float)p_frac, 4.5f, (p_frac > 0.90) ? COL_LIME : COL_CYAN, 255);

        uint16_t st_col = (!ipc_ok) ? COL_HOT_RED : (paused ? COL_AMBER : COL_CYBER_GREEN);
        ui_draw_chamfer_pill(59, 54, 122, 16, (!ipc_ok) ? COL_CYBER_WARN : COL_CYBER_PILL, st_col);
        ui_fill_circle(67.0f, 62.0f, 2.6f, st_col, 255);
        if (!ipc_ok) snprintf(pill_buf, sizeof(pill_buf), "IPC // OFFLINE");
        else if (paused) snprintf(pill_buf, sizeof(pill_buf), "PAUSED // IDLE");
        else snprintf(pill_buf, sizeof(pill_buf), "MINING // %s", mode_up);
        const ui_font_t *pf = fit_font_or_fallback(&font_space_8, &font_space_8, 98, pill_buf);
        draw_text_xy(pf, 74, 57, pill_buf, st_col);

        draw_text_centered(&font_orbitron_29, 120, 80, hr_buf, COL_WHITE);
        draw_text_centered(&font_orbitron_12, 85, 148, temp_buf, t_col);
        draw_text_centered(&font_orbitron_12, 155, 148, pwr_buf, COL_CYAN);

        if (eff > 0.0) snprintf(foot1, sizeof(foot1), "%s SHARES \xe2\x80\xa2 %.1f J/TH", shares_str ? shares_str : "0", eff);
        else snprintf(foot1, sizeof(foot1), "%s SHARES", shares_str ? shares_str : "0");
        draw_text_centered(&font_space_8, 120, 174, foot1, COL_CYBER_LIGHT);
        snprintf(foot2, sizeof(foot2), "%u/12 CHIPS OK", asics);
        draw_text_centered(&font_space_8, 120, 189, foot2, COL_CYBER_DIM);
    }

    blit_ui_fb(ui_fb);
}

static void refresh_tuner_themed(ui_theme_t theme) {
    double chips[12] = {0};
    int n_chips = 0;
    const char *csv = kv_str("chip_temps");
    if (csv && *csv) {
        char copy[160];
        strncpy(copy, csv, sizeof(copy) - 1);
        copy[sizeof(copy) - 1] = '\0';
        char *saveptr = NULL;
        char *tok = strtok_r(copy, ",", &saveptr);
        while (tok && n_chips < 12) {
            chips[n_chips++] = atof(tok);
            tok = strtok_r(NULL, ",", &saveptr);
        }
    }

    const char *mode_raw = kv_str("autotune_mode");
    char mode_disp[32] = "OFF";
    if (mode_raw && *mode_raw) uppercase_str(mode_disp, sizeof(mode_disp), mode_raw);
    int cont = kv_bool("autotune_continuous");

    double min_t = 0.0, max_t = 0.0, avg_t = 0.0;
    int have_min = kv_f64("temp_min", &min_t);
    int have_max = kv_f64("temp_max", &max_t);
    int have_avg = kv_f64("temp_avg", &avg_t);
    const char *min_chip = kv_str("temp_min_chip");
    const char *max_chip = kv_str("temp_max_chip");

    char min_lbl[24], min_val[24], max_lbl[24], max_val[24], avg_buf[36], cont_buf[32];

    if (theme == THEME_MINIMAL) {
        memcpy(ui_fb, bg_tuner_minimal, sizeof(ui_fb));
        for (int i = 0; i < 12; i++) {
            double t = (i < n_chips) ? chips[i] : 60.0;
            double tn = (t - 55.0) / 35.0; if (tn < 0.0) tn = 0.0; if (tn > 1.0) tn = 1.0;
            float thick = 5.0f + 6.0f * (float)tn;
            float a1 = -90.0f + (float)i * 30.0f + 6.5f;
            uint16_t col = (i < n_chips) ? minimal_temp_color(t) : COL_MIN_SUB;
            ui_draw_smooth_arc(110.5f - thick * 0.5f, a1, 17.0f, thick, col, col, 255);
        }
        const ui_font_t *mf = fit_font_or_fallback(&font_bahn_22, &font_bahn_13, 126, mode_disp);
        draw_text_centered(mf, 120, (mf == &font_bahn_22) ? 77 : 82, mode_disp, COL_MIN_ROYAL);

        ui_fill_circle(67.0f, 123.0f, 2.8f, cont ? COL_MIN_MINT : COL_MIN_SUB, 255);
        snprintf(cont_buf, sizeof(cont_buf), "Continuous \xe2\x80\xa2 %s", cont ? "ON" : "OFF");
        draw_text_xy(&font_segoe_8, 75, 117, cont_buf, COL_MIN_INK);

        snprintf(min_lbl, sizeof(min_lbl), "%s \xe2\x80\xa2 Cool", (min_chip && *min_chip) ? min_chip : "MIN");
        snprintf(max_lbl, sizeof(max_lbl), "%s \xe2\x80\xa2 Hot", (max_chip && *max_chip) ? max_chip : "MAX");
        if (have_min) snprintf(min_val, sizeof(min_val), "%.1f\xc2\xb0""C", min_t); else snprintf(min_val, sizeof(min_val), "--");
        if (have_max) snprintf(max_val, sizeof(max_val), "%.1f\xc2\xb0""C", max_t); else snprintf(max_val, sizeof(max_val), "--");

        draw_text_centered(&font_segoe_8, 84, 143, min_lbl, COL_MIN_SUB);
        draw_text_centered(&font_bahn_13, 84, 155, min_val, COL_MIN_MINT);
        draw_text_centered(&font_segoe_8, 156, 143, max_lbl, COL_MIN_SUB);
        draw_text_centered(&font_bahn_13, 156, 155, max_val, COL_MIN_HOT);

        if (have_avg) snprintf(avg_buf, sizeof(avg_buf), "Avg Chip \xe2\x80\xa2 %.1f\xc2\xb0""C", avg_t);
        else snprintf(avg_buf, sizeof(avg_buf), "Avg Chip \xe2\x80\xa2 --");
        draw_text_centered(&font_segoe_8, 120, 185, avg_buf, COL_MIN_SUB);
    } else if (theme == THEME_PARASITE) {
        memcpy(ui_fb, bg_tuner_parasite, sizeof(ui_fb));
        for (int i = 0; i < 12; i++) {
            double t = (i < n_chips) ? chips[i] : 60.0;
            double tn = (t - 55.0) / 35.0; if (tn < 0.0) tn = 0.0; if (tn > 1.0) tn = 1.0;
            float thick = 5.0f + 7.0f * (float)tn;
            float a1 = -90.0f + (float)i * 30.0f + 4.0f;
            uint16_t col = (i < n_chips) ? parasite_temp_color(t) : COL_PARA_DARK;
            ui_draw_wedge_arc(111.0f - thick * 0.5f, a1, 22.0f, thick, col, 255);
        }
        const ui_font_t *mf = fit_font_or_fallback(&font_courier_20, &font_courier_13, 122, mode_disp);
        draw_text_centered(mf, 120, (mf == &font_courier_20) ? 77 : 81, mode_disp, COL_WHITE);

        ui_fill_rect(54, 114, 132, 15, COL_WHITE);
        snprintf(cont_buf, sizeof(cont_buf), "[ CONT. TUNING: %s ]", cont ? "ON" : "OFF");
        draw_text_centered(&font_courier_8, 120, 117, cont_buf, COL_BLACK);

        snprintf(min_lbl, sizeof(min_lbl), "MIN [%s]", (min_chip && *min_chip) ? min_chip : "--");
        snprintf(max_lbl, sizeof(max_lbl), "MAX [%s]", (max_chip && *max_chip) ? max_chip : "--");
        if (have_min) snprintf(min_val, sizeof(min_val), "%.1fC", min_t); else snprintf(min_val, sizeof(min_val), "--");
        int hot_warn = (have_max && max_t >= 84.0);
        if (have_max) snprintf(max_val, sizeof(max_val), "%.1fC%s", max_t, hot_warn ? "!" : ""); else snprintf(max_val, sizeof(max_val), "--");

        ui_draw_rect_outline(123, 138, 66, 38, 1, hot_warn ? COL_PARA_RED : 0xD69A);
        draw_text_centered(&font_courier_8, 84, 143, min_lbl, COL_PARA_DIM);
        draw_text_centered(&font_courier_13, 84, 155, min_val, COL_WHITE);
        draw_text_centered(&font_courier_8, 156, 143, max_lbl, hot_warn ? COL_PARA_RED : COL_PARA_DIM);
        draw_text_centered(&font_courier_13, 156, 155, max_val, hot_warn ? COL_PARA_RED : COL_WHITE);

        if (have_avg) snprintf(avg_buf, sizeof(avg_buf), "AVG_CHIP: %.1fC", avg_t);
        else snprintf(avg_buf, sizeof(avg_buf), "AVG_CHIP: --");
        draw_text_centered(&font_courier_8, 120, 184, avg_buf, COL_PARA_DIM);
    } else {
        /* THEME_CYBERPUNK */
        memcpy(ui_fb, bg_tuner_cyber, sizeof(ui_fb));
        for (int i = 0; i < 12; i++) {
            double t = (i < n_chips) ? chips[i] : 60.0;
            double tn = (t - 55.0) / 35.0; if (tn < 0.0) tn = 0.0; if (tn > 1.0) tn = 1.0;
            float thick = 5.0f + 7.0f * (float)tn;
            float a1 = -90.0f + (float)i * 30.0f + 4.0f;
            uint16_t col = (i < n_chips) ? cyber_temp_color(t) : COL_CYBER_TRACK;
            ui_draw_wedge_arc(111.0f - thick * 0.5f, a1, 22.0f, thick, col, 255);
        }
        const ui_font_t *mf = fit_font_or_fallback(&font_orbitron_19, &font_orbitron_12, 124, mode_disp);
        draw_text_centered(mf, 120, (mf == &font_orbitron_19) ? 78 : 82, mode_disp, COL_WHITE);

        uint16_t cc = cont ? COL_CYBER_GREEN : COL_CYBER_DIM;
        ui_draw_chamfer_pill(55, 115, 130, 17, cont ? COL_CYBER_PILL : COL_CYBER_TRACK, cc);
        ui_fill_circle(64.0f, 123.5f, 2.6f, cc, 255);
        snprintf(cont_buf, sizeof(cont_buf), "CONTINUOUS : %s", cont ? "ON" : "OFF");
        draw_text_xy(&font_space_8, 71, 118, cont_buf, cc);

        snprintf(min_lbl, sizeof(min_lbl), "COOL \xe2\x80\xa2 %s", (min_chip && *min_chip) ? min_chip : "--");
        snprintf(max_lbl, sizeof(max_lbl), "HOT \xe2\x80\xa2 %s", (max_chip && *max_chip) ? max_chip : "--");
        if (have_min) snprintf(min_val, sizeof(min_val), "%.1f\xc2\xb0""C", min_t); else snprintf(min_val, sizeof(min_val), "--");
        if (have_max) snprintf(max_val, sizeof(max_val), "%.1f\xc2\xb0""C", max_t); else snprintf(max_val, sizeof(max_val), "--");

        draw_text_centered(&font_space_8, 84, 143, min_lbl, COL_CYBER_DIM);
        draw_text_centered(&font_orbitron_12, 84, 155, min_val, COL_CYBER_GREEN);
        draw_text_centered(&font_space_8, 156, 143, max_lbl, COL_CYBER_DIM);
        draw_text_centered(&font_orbitron_12, 156, 155, max_val, have_max ? cyber_temp_color(max_t) : COL_HOT_RED);

        if (have_avg) snprintf(avg_buf, sizeof(avg_buf), "AVG CHIP: %.1f\xc2\xb0""C", avg_t);
        else snprintf(avg_buf, sizeof(avg_buf), "AVG CHIP: --");
        draw_text_centered(&font_space_8, 120, 185, avg_buf, COL_CYBER_LIGHT);
    }

    blit_ui_fb(ui_fb);
}

static void refresh_ip_themed(ui_theme_t theme) {
    char ip[32] = "";
    int online = wlan_ip(ip, sizeof(ip));
    int rssi = -48;
    int have_rssi = wlan_rssi_dbm(&rssi);

    /* Map RSSI [-90..-30 dBm] to 1..6 signal segments */
    int sig_segs = 0;
    if (online) {
        if (!have_rssi) sig_segs = 5;
        else if (rssi >= -50) sig_segs = 6;
        else if (rssi >= -60) sig_segs = 5;
        else if (rssi >= -70) sig_segs = 4;
        else if (rssi >= -80) sig_segs = 3;
        else sig_segs = 2;
    }

    char rssi_buf[24], url_buf[48];
    if (online && have_rssi) snprintf(rssi_buf, sizeof(rssi_buf), "%d dBm", rssi);
    else if (online) snprintf(rssi_buf, sizeof(rssi_buf), "GOOD");
    else snprintf(rssi_buf, sizeof(rssi_buf), "-- dBm");
    if (online) snprintf(url_buf, sizeof(url_buf), "http://%s", ip);

    if (theme == THEME_MINIMAL) {
        memcpy(ui_fb, bg_ip_minimal, sizeof(ui_fb));
        float sig_sweep = online ? (110.0f * (float)sig_segs / 6.0f) : 18.0f;
        ui_draw_smooth_arc(107.0f, 125.0f, sig_sweep, 6.5f, online ? COL_MIN_MINT : COL_MIN_HOT, online ? COL_MIN_MINT : COL_MIN_HOT, 255);
        ui_draw_smooth_arc(107.0f, 305.0f, online ? 110.0f : 22.0f, 6.5f, COL_MIN_ROYAL, COL_MIN_SKY, 255);

        ui_fill_circle(71.0f, 65.5f, 2.8f, online ? COL_MIN_MINT : COL_MIN_HOT, 255);
        draw_text_xy(&font_segoe_8, 79, 59, online ? "wlan0 \xe2\x80\xa2 Connected" : "wlan0 \xe2\x80\xa2 No WiFi", COL_MIN_INK);

        const char *ip_txt = online ? ip : "NO WIFI";
        const ui_font_t *ipf = fit_font_or_fallback(&font_bahn_22, &font_bahn_13, 146, ip_txt);
        draw_text_centered(ipf, 120, 97, ip_txt, online ? COL_MIN_ROYAL : COL_MIN_HOT);

        draw_text_centered(&font_bahn_13, 84, 155, rssi_buf, COL_MIN_MINT);
        draw_text_centered(&font_bahn_13, 156, 155, "Port 80", COL_MIN_ROYAL);
        draw_text_centered(&font_segoe_8, 120, 184, online ? url_buf : "Waiting for DHCP", COL_MIN_SUB);
    } else if (theme == THEME_PARASITE) {
        memcpy(ui_fb, bg_ip_parasite, sizeof(ui_fb));
        int on_ticks = online ? ((sig_segs * 14) / 6) : 0;
        ui_draw_radial_ticks(14, 135.0f, 90.0f, 106.0f, 103.0f, 111.0f, 3, on_ticks, COL_WHITE, COL_PARA_RED, -1, COL_PARA_DARK, 1.1f, 0.6f);
        ui_draw_radial_ticks(14, 315.0f, 90.0f, 106.0f, 103.0f, 111.0f, 3, online ? 14 : 2, COL_WHITE, COL_PARA_RED, -1, COL_PARA_DARK, 1.1f, 0.6f);

        ui_fill_rect(56, 57, 128, 15, online ? COL_WHITE : COL_PARA_RED);
        draw_text_centered(&font_courier_8, 120, 60, online ? "[ WLAN0 : BOUND ]" : "[ NO WIFI LINK ]", COL_BLACK);

        const char *ip_txt = online ? ip : "DISCONNECTED";
        const ui_font_t *ipf = fit_font_or_fallback(&font_courier_20, &font_courier_13, 148, ip_txt);
        draw_text_centered(ipf, 120, 98, ip_txt, online ? COL_WHITE : COL_PARA_RED);

        if (online && have_rssi) snprintf(rssi_buf, sizeof(rssi_buf), "%ddBm", rssi);
        draw_text_centered(&font_courier_13, 84, 155, rssi_buf, COL_WHITE);
        draw_text_centered(&font_courier_13, 156, 155, "PORT 80", COL_WHITE);
        if (online) snprintf(url_buf, sizeof(url_buf), "> http://%s", ip); else snprintf(url_buf, sizeof(url_buf), "> STA_RETRY...");
        draw_text_centered(&font_courier_8, 120, 184, url_buf, COL_WHITE);
    } else {
        /* THEME_CYBERPUNK */
        memcpy(ui_fb, bg_ip_cyber, sizeof(ui_fb));
        float span = 96.0f, gap = 3.0f, seg = (span - 5.0f * gap) / 6.0f;
        for (int i = 0; i < 6; i++) {
            float a1 = 132.0f + (float)i * (seg + gap);
            uint16_t sc = (i < sig_segs) ? COL_CYBER_GREEN : (online ? COL_CYBER_TRACK : COL_HOT_RED);
            ui_draw_wedge_arc(109.0f, a1, seg, 6.0f, sc, 255);
            float a2 = 312.0f + (float)i * (seg + gap);
            uint16_t lc = online ? ((i == 5) ? COL_MAG : COL_CYAN) : COL_CYBER_TRACK;
            ui_draw_wedge_arc(109.0f, a2, seg, 6.0f, lc, 255);
        }

        uint16_t st_col = online ? COL_CYBER_GREEN : COL_HOT_RED;
        ui_draw_chamfer_pill(59, 57, 122, 16, online ? COL_CYBER_PILL : COL_CYBER_WARN, st_col);
        ui_fill_circle(68.0f, 65.0f, 2.6f, st_col, 255);
        draw_text_xy(&font_space_8, 75, 60, online ? "WLAN0 // ONLINE" : "NO WIFI LINK", st_col);

        const char *ip_txt = online ? ip : "DISCONNECTED";
        const ui_font_t *ipf = fit_font_or_fallback(&font_orbitron_19, &font_orbitron_12, 146, ip_txt);
        draw_text_centered(ipf, 120, 98, ip_txt, online ? COL_WHITE : COL_HOT_RED);

        draw_text_centered(&font_orbitron_12, 84, 155, rssi_buf, online ? COL_CYBER_GREEN : COL_CYBER_DIM);
        draw_text_centered(&font_orbitron_12, 156, 155, "HTTP :80", COL_CYAN);
        draw_text_centered(&font_space_8, 120, 184, online ? url_buf : "STA MODE RETRY", COL_CYBER_LIGHT);
    }

    blit_ui_fb(ui_fb);
}

/* ── Animated Themed Bluetooth WiFi Setup Screen ─────────────────────── */

static void ui_draw_aa_line(float x0, float y0, float x1, float y1, float half_w, uint16_t col, uint8_t opa) {
    if (opa == 0 || half_w <= 0.0f) return;
    float minx = (x0 < x1 ? x0 : x1) - half_w - 1.5f;
    float maxx = (x0 > x1 ? x0 : x1) + half_w + 1.5f;
    float miny = (y0 < y1 ? y0 : y1) - half_w - 1.5f;
    float maxy = (y0 > y1 ? y0 : y1) + half_w + 1.5f;
    int ix0 = (int)floorf(minx); if (ix0 < 0) ix0 = 0;
    int ix1 = (int)ceilf(maxx);  if (ix1 >= HOR_RES) ix1 = HOR_RES - 1;
    int iy0 = (int)floorf(miny); if (iy0 < 0) iy0 = 0;
    int iy1 = (int)ceilf(maxy);  if (iy1 >= VER_RES) iy1 = VER_RES - 1;
    float vx = x1 - x0, vy = y1 - y0;
    float len_sq = vx * vx + vy * vy;
    for (int y = iy0; y <= iy1; y++) {
        for (int x = ix0; x <= ix1; x++) {
            float px = (float)x - x0, py = (float)y - y0;
            float t = (len_sq > 1e-5f) ? ((px * vx + py * vy) / len_sq) : 0.0f;
            if (t < 0.0f) t = 0.0f;
            if (t > 1.0f) t = 1.0f;
            float dx = px - t * vx, dy = py - t * vy;
            float dist = sqrtf(dx * dx + dy * dy);
            if (dist <= half_w + 0.75f) {
                float cov = (dist <= half_w - 0.45f) ? 1.0f : (half_w + 0.75f - dist) / 1.2f;
                if (cov > 1.0f) cov = 1.0f;
                uint8_t a = (uint8_t)(cov * (float)opa);
                if (a > 0) {
                    int idx = y * HOR_RES + x;
                    ui_fb[idx] = blend565(ui_fb[idx], col, a);
                }
            }
        }
    }
}

static void ui_draw_local_arc(float cx, float cy, float r_mid, float start_deg, float sweep_deg,
                              float half_w, uint16_t col, uint8_t opa) {
    if (opa == 0 || sweep_deg <= 0.1f || half_w <= 0.0f) return;
    float r_max = r_mid + half_w + 1.5f;
    int ix0 = (int)floorf(cx - r_max); if (ix0 < 0) ix0 = 0;
    int ix1 = (int)ceilf(cx + r_max);  if (ix1 >= HOR_RES) ix1 = HOR_RES - 1;
    int iy0 = (int)floorf(cy - r_max); if (iy0 < 0) iy0 = 0;
    int iy1 = (int)ceilf(cy + r_max);  if (iy1 >= VER_RES) iy1 = VER_RES - 1;
    float s_norm = wrap360(start_deg);
    float s_rad = s_norm * 0.0174532925f;
    float e_rad = wrap360(s_norm + sweep_deg) * 0.0174532925f;
    float cx_s = cx + r_mid * cosf(s_rad), cy_s = cy + r_mid * sinf(s_rad);
    float cx_e = cx + r_mid * cosf(e_rad), cy_e = cy + r_mid * sinf(e_rad);
    float max_d = half_w + 0.75f;
    float max_d_sq = max_d * max_d;
    for (int y = iy0; y <= iy1; y++) {
        float dy = (float)y - cy;
        for (int x = ix0; x <= ix1; x++) {
            float dx = (float)x - cx;
            float r = sqrtf(dx * dx + dy * dy);
            float dr = fabsf(r - r_mid);
            if (dr > max_d) continue;
            float deg = atan2f(dy, dx) * 57.2957795f;
            if (deg < 0.0f) deg += 360.0f;
            float rel = deg - s_norm;
            if (rel < 0.0f) rel += 360.0f;
            float dist;
            if (sweep_deg >= 359.9f || rel <= sweep_deg) {
                dist = dr;
            } else {
                float d1_sq = ((float)x - cx_s) * ((float)x - cx_s) + ((float)y - cy_s) * ((float)y - cy_s);
                float d2_sq = ((float)x - cx_e) * ((float)x - cx_e) + ((float)y - cy_e) * ((float)y - cy_e);
                float min_sq = (d1_sq < d2_sq) ? d1_sq : d2_sq;
                if (min_sq > max_d_sq) continue;
                dist = sqrtf(min_sq);
            }
            float cov = (dist <= half_w - 0.45f) ? 1.0f : (max_d - dist) / 1.2f;
            if (cov > 1.0f) cov = 1.0f;
            uint8_t a = (uint8_t)(cov * (float)opa);
            if (a > 0) {
                int idx = y * HOR_RES + x;
                ui_fb[idx] = blend565(ui_fb[idx], col, a);
            }
        }
    }
}

static void ui_draw_bt_symbol(float cx, float cy, float scale, uint16_t rune_col, uint16_t glow_col,
                              uint16_t wave_col1, uint16_t wave_col2, float t_sec) {
    /* Two staggered pairs of expanding RF broadcast arcs on left & right */
    for (int w = 0; w < 2; w++) {
        float phase = fmodf(t_sec * 0.85f + (float)w * 0.5f, 1.0f);
        float r_arc = (13.5f + phase * 17.5f) * scale;
        float fade = 1.0f - phase;
        uint8_t w_opa = (uint8_t)(fade * fade * 235.0f);
        uint16_t w_col = (w == 0) ? wave_col1 : wave_col2;
        if (w_opa > 4) {
            ui_draw_local_arc(cx, cy, r_arc, 148.0f, 64.0f, 1.15f * scale, w_col, w_opa);
            ui_draw_local_arc(cx, cy, r_arc, 328.0f, 64.0f, 1.15f * scale, w_col, w_opa);
        }
    }

    float hh = 14.0f * scale;
    float hw = 7.5f * scale;
    float hy = hh * 0.48f;
    float stroke = 1.3f * scale;

    /* Optional subtle breathing halo behind the Bluetooth bind-rune */
    if (glow_col != rune_col) {
        float pulse = 0.5f + 0.5f * sinf(t_sec * 3.14159265f);
        uint8_t g_opa = (uint8_t)(55.0f + pulse * 65.0f);
        float gs = stroke + 1.4f * scale;
        ui_draw_aa_line(cx, cy - hh, cx, cy + hh, gs, glow_col, g_opa);
        ui_draw_aa_line(cx, cy - hh, cx + hw, cy - hy, gs, glow_col, g_opa);
        ui_draw_aa_line(cx + hw, cy - hy, cx - hw, cy + hy, gs, glow_col, g_opa);
        ui_draw_aa_line(cx, cy + hh, cx + hw, cy + hy, gs, glow_col, g_opa);
        ui_draw_aa_line(cx + hw, cy + hy, cx - hw, cy - hy, gs, glow_col, g_opa);
    }

    /* Crisp Bluetooth bind-rune (5 segments) */
    ui_draw_aa_line(cx, cy - hh, cx, cy + hh, stroke, rune_col, 255);
    ui_draw_aa_line(cx, cy - hh, cx + hw, cy - hy, stroke, rune_col, 255);
    ui_draw_aa_line(cx + hw, cy - hy, cx - hw, cy + hy, stroke, rune_col, 255);
    ui_draw_aa_line(cx, cy + hh, cx + hw, cy + hy, stroke, rune_col, 255);
    ui_draw_aa_line(cx + hw, cy + hy, cx - hw, cy - hy, stroke, rune_col, 255);
}

static uint16_t ble_bg_cache[3][HOR_RES * VER_RES];
static int ble_bg_inited[3] = {0, 0, 0};

static void init_ble_bg_theme(ui_theme_t theme) {
    if (ble_bg_inited[theme]) return;
    if (theme == THEME_MINIMAL) {
        for (int y = 0; y < VER_RES; y++) {
            for (int x = 0; x < HOR_RES; x++) {
                float dx1 = (float)(x - 62), dy1 = (float)(y - 62);
                float dx2 = (float)(x - 182), dy2 = (float)(y - 105);
                float dx3 = (float)(x - 125), dy3 = (float)(y - 190);
                float w1 = expf(-(dx1 * dx1 + dy1 * dy1) / 5600.0f);
                float w2 = expf(-(dx2 * dx2 + dy2 * dy2) / 6200.0f);
                float w3 = expf(-(dx3 * dx3 + dy3 * dy3) / 6000.0f);
                float r = 216.0f - 38.0f * w1 + 4.0f * w2 + 34.0f * w3;
                float g = 232.0f - 18.0f * w1 - 28.0f * w2 - 10.0f * w3;
                float b = 252.0f + 3.0f * w1 - 2.0f * w2 - 16.0f * w3;
                if (r < 0.0f) r = 0.0f; if (r > 255.0f) r = 255.0f;
                if (g < 0.0f) g = 0.0f; if (g > 255.0f) g = 255.0f;
                if (b < 0.0f) b = 0.0f; if (b > 255.0f) b = 255.0f;
                ui_fb[y * HOR_RES + x] = RGB565((int)r, (int)g, (int)b);
            }
        }
        /* Frosted outer ring track + hero medallion + rounded info glass card */
        ui_draw_smooth_arc(107.0f, 0.0f, 360.0f, 6.0f, RGB565(225, 236, 250), RGB565(225, 236, 250), 190);
        ui_fill_circle(120.0f, 65.5f, 23.0f, RGB565(155, 175, 205), 85);
        ui_fill_circle(120.0f, 63.0f, 23.0f, COL_WHITE, 210);
        ui_fill_circle(120.0f, 63.0f, 22.0f, RGB565(245, 250, 255), 180);
        ui_fill_rect(42, 118, 156, 56, RGB565(248, 251, 255));
        ui_draw_rect_outline(42, 118, 156, 56, 1, COL_WHITE);
    } else if (theme == THEME_PARASITE) {
        memset(ui_fb, 0, sizeof(ui_fb));
        ui_draw_rect_outline(40, 116, 160, 58, 1, 0xD69A);
    } else {
        /* THEME_CYBERPUNK */
        uint16_t bg_dark = RGB565(5, 6, 12);
        for (int i = 0; i < HOR_RES * VER_RES; i++) ui_fb[i] = bg_dark;
        uint16_t grid_c = RGB565(0, 32, 40);
        for (int x = 12; x < HOR_RES; x += 18) ui_fill_rect(x, 0, 1, VER_RES, grid_c);
        for (int y = 12; y < VER_RES; y += 18) ui_fill_rect(0, y, HOR_RES, 1, grid_c);
        ui_fill_circle(120.0f, 63.0f, 25.0f, RGB565(8, 28, 44), 175);
        ui_draw_chamfer_pill(38, 116, 164, 58, RGB565(10, 15, 28), COL_CYAN);
        ui_fill_rect(38, 121, 2, 48, COL_CYAN);
        for (int y = 0; y < VER_RES; y += 3) {
            for (int x = 0; x < HOR_RES; x++) {
                ui_fb[y * HOR_RES + x] = blend565(ui_fb[y * HOR_RES + x], COL_BLACK, 45);
            }
        }
    }
    memcpy(ble_bg_cache[theme], ui_fb, sizeof(ui_fb));
    ble_bg_inited[theme] = 1;
}

static lv_obj_t *scr_waiting, *waiting_label;

static lv_obj_t *make_label(lv_obj_t *parent, const lv_font_t *font, lv_color_t color, lv_coord_t y, const char *text) {
    lv_obj_t *l = lv_label_create(parent);
    lv_obj_set_style_text_font(l, font, LV_PART_MAIN);
    lv_obj_set_style_text_color(l, color, LV_PART_MAIN);
    lv_obj_align(l, LV_ALIGN_TOP_MID, 0, y);
    if (text) lv_label_set_text(l, text);
    return l;
}

static void build_screens(void) {
    const lv_font_t *f24 = &lv_font_montserrat_24;

    scr_waiting = lv_obj_create(NULL);
    lv_obj_set_style_bg_color(scr_waiting, C_BG, LV_PART_MAIN);
    lv_obj_set_style_bg_opa(scr_waiting, LV_OPA_COVER, LV_PART_MAIN);
    waiting_label = make_label(scr_waiting, f24, C_TEXT_1, 108, "STARTING");
}

static void refresh_wifi_setup_themed(ui_theme_t theme, uint32_t ms) {
    static uint32_t last_kv_poll = 0;
    static int kv_ready = 0;
    static char cached_state[32] = "advertising";
    static char cached_detail[64] = "nan3s_XXXX";

    if (!kv_ready || ms - last_kv_poll >= 400u) {
        last_kv_poll = ms;
        kv_ready = 1;
        if (read_kv_file(BLE_WIFI_STATUS_FILE)) {
            const char *st = kv_str("state");
            const char *dt = kv_str("detail");
            if (st && *st) snprintf(cached_state, sizeof(cached_state), "%s", st);
            else snprintf(cached_state, sizeof(cached_state), "advertising");
            if (dt && *dt) snprintf(cached_detail, sizeof(cached_detail), "%s", dt);
            else cached_detail[0] = '\0';
        } else {
            snprintf(cached_state, sizeof(cached_state), "advertising");
            snprintf(cached_detail, sizeof(cached_detail), "nan3s_XXXX");
        }
    }

    init_ble_bg_theme(theme);
    memcpy(ui_fb, ble_bg_cache[theme], sizeof(ui_fb));

    const char *pill_txt = "ADVERTISING";
    const char *headline = "Waiting for phone";
    char sub1[48] = "Open Avalon Life";
    char sub2[48] = "";

    int st_kind = 0; /* 0=advertising/active, 1=connected/ok, 2=failed */
    float progress = 0.25f;
    if (strcmp(cached_state, "client_connected") == 0) {
        pill_txt = "LINKED";
        headline = "Phone connected";
        snprintf(sub1, sizeof(sub1), "Awaiting WiFi config");
        progress = 0.45f;
    } else if (strcmp(cached_state, "credentials_received") == 0) {
        pill_txt = "SAVED";
        headline = "WiFi info received";
        snprintf(sub1, sizeof(sub1), "%s", cached_detail[0] ? cached_detail : "Credentials saved");
        progress = 0.65f;
    } else if (strcmp(cached_state, "connecting") == 0) {
        pill_txt = "CONNECTING";
        headline = "Connecting...";
        snprintf(sub1, sizeof(sub1), "%s", cached_detail[0] ? cached_detail : "Associating WLAN0");
        progress = 0.85f;
    } else if (strcmp(cached_state, "connected") == 0) {
        st_kind = 1;
        pill_txt = "ONLINE";
        headline = "WiFi connected!";
        if (cached_detail[0]) snprintf(sub1, sizeof(sub1), "IP %s", cached_detail);
        else snprintf(sub1, sizeof(sub1), "WLAN0 bound");
        progress = 1.0f;
    } else if (strcmp(cached_state, "failed") == 0) {
        st_kind = 2;
        pill_txt = "FAILED";
        headline = "Setup failed";
        snprintf(sub1, sizeof(sub1), "Check password,");
        snprintf(sub2, sizeof(sub2), "try again");
        progress = 1.0f;
    } else {
        pill_txt = "ADVERTISING";
        headline = "Waiting for phone";
        snprintf(sub1, sizeof(sub1), "Open Avalon Life");
        if (cached_detail[0]) snprintf(sub2, sizeof(sub2), "Look for %s", cached_detail);
        progress = 0.25f;
    }

    float t = (float)(ms % 120000u) * 0.001f;

    if (theme == THEME_MINIMAL) {
        uint16_t acc_col = (st_kind == 1) ? COL_MIN_MINT : (st_kind == 2 ? COL_MIN_HOT : COL_MIN_ROYAL);
        uint16_t wave1 = (st_kind == 1) ? COL_MIN_MINT : (st_kind == 2 ? COL_MIN_WARM : COL_MIN_SKY);
        uint16_t wave2 = (st_kind == 1) ? COL_MIN_MINT_BRT : (st_kind == 2 ? COL_MIN_HOT : COL_MIN_BLUE);
        float rot = fmodf(t * 36.0f, 360.0f);
        ui_draw_smooth_arc(107.0f, -90.0f, 360.0f * progress, 6.0f, acc_col, wave1, 140);
        ui_draw_smooth_arc(107.0f, rot, 95.0f, 6.0f, acc_col, wave1, 255);
        ui_draw_smooth_arc(107.0f, rot + 180.0f, 70.0f, 6.0f, COL_MIN_MINT, COL_MIN_MINT_BRT, 220);

        ui_draw_bt_symbol(120.0f, 63.0f, 0.92f, acc_col, acc_col, wave1, wave2, t);

        char pill_buf[40];
        snprintf(pill_buf, sizeof(pill_buf), "BLE \xe2\x80\xa2 %s", pill_txt);
        int pw = measure_text_px(&font_segoe_8, pill_buf) + 22;
        if (pw < 98) pw = 98;
        int px = 120 - pw / 2;
        ui_fill_rect(px, 95, pw, 16, RGB565(250, 252, 255));
        ui_draw_rect_outline(px, 95, pw, 16, 1, COL_WHITE);
        ui_fill_circle((float)(px + 9), 103.0f, 2.8f, acc_col, 255);
        draw_text_xy(&font_segoe_8, px + 16, 97, pill_buf, COL_MIN_INK);

        const ui_font_t *hf = fit_font_or_fallback(&font_bahn_13, &font_segoe_8, 146, headline);
        draw_text_centered(hf, 120, 123, headline, acc_col);
        draw_text_centered(&font_segoe_8, 120, 144, sub1, COL_MIN_INK);
        if (sub2[0]) draw_text_centered(&font_segoe_8, 120, 158, sub2, COL_MIN_SUB);
        draw_text_centered(&font_segoe_8, 120, 184, "WiFi Provisioning", COL_MIN_SUB);
    } else if (theme == THEME_PARASITE) {
        uint16_t acc_col = (st_kind == 2) ? COL_PARA_RED : COL_WHITE;
        float rot_ticks = fmodf(t * 20.0f, 360.0f);
        float rot_scan = fmodf(-t * 52.0f, 360.0f);
        int active_n = (int)roundf(progress * 36.0f);
        ui_draw_radial_ticks(36, rot_ticks, 360.0f, 106.0f, 102.5f, 111.5f, 3, active_n,
                             COL_WHITE, COL_PARA_RED, (st_kind == 2) ? 0 : 28, COL_PARA_DARK, 1.0f, 0.6f);
        ui_draw_wedge_arc(98.5f, rot_scan, 52.0f, 2.6f, COL_WHITE, 255);
        ui_draw_wedge_arc(98.5f, rot_scan + 180.0f, 52.0f, 2.6f, COL_PARA_RED, 255);

        ui_draw_bt_symbol(120.0f, 62.0f, 0.95f, acc_col, acc_col, COL_WHITE, COL_PARA_RED, t);

        char pill_buf[40], head_up[40];
        snprintf(pill_buf, sizeof(pill_buf), "BLE // %s", pill_txt);
        uppercase_str(head_up, sizeof(head_up), headline);
        ui_fill_rect(54, 95, 132, 15, (st_kind == 2) ? COL_PARA_RED : COL_WHITE);
        draw_text_centered(&font_courier_8, 120, 98, pill_buf, COL_BLACK);

        const ui_font_t *hf = fit_font_or_fallback(&font_courier_13, &font_courier_8, 150, head_up);
        draw_text_centered(hf, 120, 123, head_up, acc_col);
        draw_text_centered(&font_courier_8, 120, 144, sub1, COL_WHITE);
        if (sub2[0]) draw_text_centered(&font_courier_8, 120, 158, sub2, COL_PARA_DIM);
        draw_text_centered(&font_courier_8, 120, 184, "[ BLE_PROV // INIT ]", COL_PARA_DIM);
    } else {
        /* THEME_CYBERPUNK */
        uint16_t acc_col = (st_kind == 1) ? COL_CYBER_GREEN : (st_kind == 2 ? COL_HOT_RED : COL_CYAN);
        uint16_t wave2 = (st_kind == 1) ? COL_CYAN : (st_kind == 2 ? COL_AMBER : COL_MAG);
        float rot1 = fmodf(t * 45.0f, 360.0f);
        float rot2 = fmodf(-t * 30.0f, 360.0f);
        ui_draw_wedge_arc(109.0f, rot1 + 20.0f, 52.0f, 5.5f, acc_col, 255);
        ui_draw_wedge_arc(109.0f, rot1 + 108.0f, 52.0f, 5.5f, acc_col, 255);
        ui_draw_wedge_arc(109.0f, rot1 + 200.0f, 52.0f, 5.5f, acc_col, 255);
        ui_draw_wedge_arc(109.0f, rot1 + 288.0f, 52.0f, 5.5f, acc_col, 255);
        ui_draw_wedge_arc(109.0f, rot1 + 78.0f, 24.0f, 5.5f, wave2, 255);
        ui_draw_wedge_arc(109.0f, rot1 + 258.0f, 24.0f, 5.5f, wave2, 255);
        ui_draw_radial_ticks(4, rot2, 360.0f, 94.0f, 94.0f, 102.5f, 1, 4, acc_col, acc_col, -1, acc_col, 1.0f, 1.0f);

        ui_draw_bt_symbol(120.0f, 62.0f, 0.95f, COL_WHITE, acc_col, acc_col, wave2, t);

        char pill_buf[40], head_up[40];
        snprintf(pill_buf, sizeof(pill_buf), "BLE // %s", pill_txt);
        uppercase_str(head_up, sizeof(head_up), headline);
        ui_draw_chamfer_pill(56, 95, 128, 16, (st_kind == 2) ? COL_CYBER_WARN : COL_CYBER_PILL, acc_col);
        ui_fill_circle(65.0f, 103.0f, 2.5f, acc_col, 255);
        draw_text_xy(&font_space_8, 72, 98, pill_buf, acc_col);

        const ui_font_t *hf = fit_font_or_fallback(&font_orbitron_12, &font_space_8, 150, head_up);
        draw_text_centered(hf, 120, 123, head_up, COL_WHITE);
        draw_text_centered(&font_space_8, 120, 144, sub1, COL_CYBER_LIGHT);
        if (sub2[0]) draw_text_centered(&font_space_8, 120, 158, sub2, acc_col);
        draw_text_centered(&font_space_8, 120, 185, "BLE WIFI SETUP", COL_CYBER_DIM);
    }

    blit_ui_fb(ui_fb);
}

static void refresh_pizza(void) {
    static uint8_t pizza_buf[FB_BYTES];
    FILE *f = fopen(PIZZA_FILE, "rb");
    if (!f) return;
    size_t n = fread(pizza_buf, 1, sizeof(pizza_buf), f);
    fclose(f);
    if (n != sizeof(pizza_buf)) return;
    blit_ui_fb((const uint16_t *)pizza_buf);
}

/* ── IxTech Animated Logo (240x240 Circular, 15 FPS, 10.0s Loop) ──────── */
static uint8_t *ixlogo_frames = NULL;
static int ixlogo_inited = 0;

static void init_ixlogo(void) {
    if (!ixlogo_inited) {
        tinf_init();
        ixlogo_frames = (uint8_t *)malloc(IXLOGO_TOTAL_BYTES);
        if (ixlogo_frames) {
            unsigned int out_sz = IXLOGO_TOTAL_BYTES;
            int res = tinf_uncompress(ixlogo_frames, &out_sz, ixlogo_deflate, sizeof(ixlogo_deflate));
            if (res != TINF_OK || out_sz != IXLOGO_TOTAL_BYTES) {
                free(ixlogo_frames);
                ixlogo_frames = NULL;
            }
        }
        ixlogo_inited = 1;
    }
}

static void refresh_ixlogo(uint32_t ms) {
    if (!ixlogo_inited) {
        init_ixlogo();
    }

    if (!ixlogo_frames) {
        memset(ui_fb, 0, sizeof(ui_fb));
        blit_ui_fb(ui_fb);
        return;
    }

    uint32_t frame_idx = (ms / IXLOGO_FRAME_MS) % IXLOGO_NUM_FRAMES;
    const uint8_t *src = &ixlogo_frames[frame_idx * IXLOGO_FRAME_BYTES];

    for (int i = 0; i < HOR_RES * VER_RES; i++) {
        ui_fb[i] = ixlogo_palette[src[i]];
    }

    blit_ui_fb(ui_fb);
}

/* ── Procedural Plasma Disk Animation with Black Bitcoin B ──────────────── */
#define PLASMA_NUM_STREAMERS 14

/* Pure Electric Bitcoin Orange Palette (#F7931A):
 * Exact #F7931A is (247, 147, 26) -> RGB565: (30, 36, 3) -> 0xF483
 * Core: Bright intense golden-orange (255, 180, 50) -> RGB565: (31, 45, 6) -> 0xFDA6
 * Glow: Pure Bitcoin Orange #F7931A (247, 147, 26) -> RGB565: (30, 36, 3) -> 0xF483
 * Halo: Deep fiery amber (220, 85, 0) -> RGB565: (27, 21, 0) -> 0xDAA0
 * Border / Rim: Pure Bitcoin Orange #F7931A -> 0xF483
 * Spark: High-energy white-orange corona spark -> 0xFFB0
 */
#define PLASMA_COLOR_CORE   0xFDA6  /* Intense electric golden-orange */
#define PLASMA_COLOR_GLOW   0xF483  /* Exact Bitcoin Orange #F7931A */
#define PLASMA_COLOR_HALO   0xDAA0  /* Deep fiery amber */
#define PLASMA_COLOR_BORDER 0xF483  /* Pure #F7931A border */
#define PLASMA_COLOR_SPARK  0xFFB0  /* High-energy corona spark */

typedef struct {
    float x, y;
} plasma_pt_t;

typedef struct {
    float angle;
    float target_angle;
    int life;
    float intensity;
} plasma_streamer_t;

static uint16_t plasma_fb[HOR_RES * VER_RES];
static plasma_streamer_t plasma_streamers[PLASMA_NUM_STREAMERS];
static int plasma_inited = 0;

static inline uint16_t plasma_blend_add(uint16_t bg, uint16_t fg) {
    uint32_t r = ((bg >> 11) & 0x1F) + ((fg >> 11) & 0x1F);
    uint32_t g = ((bg >> 5) & 0x3F) + ((fg >> 5) & 0x3F);
    uint32_t b = (bg & 0x1F) + (fg & 0x1F);
    if (r > 31) r = 31;
    if (g > 63) g = 63;
    if (b > 31) b = 31;
    return (uint16_t)((r << 11) | (g << 5) | b);
}

static inline uint16_t plasma_decay_pixel(uint16_t p) {
    if (!p) return 0;
    /* Red decays slowest (retention ~81%), green drops faster (~56%), blue drops rapidly (~25%)
     * This keeps persistence trails warmly electric orange and fiery amber without green shift */
    uint32_t r = ((p >> 11) & 0x1F) * 26 >> 5;
    uint32_t g = ((p >> 5) & 0x3F) * 18 >> 5;
    uint32_t b = (p & 0x1F) * 8 >> 5;
    return (uint16_t)((r << 11) | (g << 5) | b);
}

static void plasma_decay_buffer(void) {
    for (int i = 0; i < HOR_RES * VER_RES; i++) {
        plasma_fb[i] = plasma_decay_pixel(plasma_fb[i]);
    }
}

static inline void plasma_put_pixel_safe(int x, int y, uint16_t col) {
    if ((unsigned)x < HOR_RES && (unsigned)y < VER_RES) {
        int idx = y * HOR_RES + x;
        plasma_fb[idx] = plasma_blend_add(plasma_fb[idx], col);
    }
}

/* Multi-tier hierarchical line drawing:
 * level 0: Primary trunk (thick, bold, 3px core, 5px glow, 7px halo)
 * level 1: Secondary branch (2px core, 4px glow)
 * level 2: Tertiary micro-filament (fine 1-2px spiderweb crackle)
 */
static void plasma_draw_line(int x0, int y0, int x1, int y1, int level) {
    int dx = abs(x1 - x0);
    int dy = abs(y1 - y0);
    int sx = (x0 < x1) ? 1 : -1;
    int sy = (y0 < y1) ? 1 : -1;
    int err = dx - dy;

    while (1) {
        if (level == 0) {
            /* Bold primary trunk */
            plasma_put_pixel_safe(x0, y0, PLASMA_COLOR_CORE);
            plasma_put_pixel_safe(x0 + 1, y0, PLASMA_COLOR_CORE);
            plasma_put_pixel_safe(x0 - 1, y0, PLASMA_COLOR_CORE);
            plasma_put_pixel_safe(x0, y0 + 1, PLASMA_COLOR_CORE);
            plasma_put_pixel_safe(x0, y0 - 1, PLASMA_COLOR_CORE);

            plasma_put_pixel_safe(x0 + 1, y0 + 1, PLASMA_COLOR_GLOW);
            plasma_put_pixel_safe(x0 - 1, y0 + 1, PLASMA_COLOR_GLOW);
            plasma_put_pixel_safe(x0 + 1, y0 - 1, PLASMA_COLOR_GLOW);
            plasma_put_pixel_safe(x0 - 1, y0 - 1, PLASMA_COLOR_GLOW);
            plasma_put_pixel_safe(x0 + 2, y0, PLASMA_COLOR_GLOW);
            plasma_put_pixel_safe(x0 - 2, y0, PLASMA_COLOR_GLOW);
            plasma_put_pixel_safe(x0, y0 + 2, PLASMA_COLOR_GLOW);
            plasma_put_pixel_safe(x0 - 2, y0, PLASMA_COLOR_GLOW);

            plasma_put_pixel_safe(x0 + 2, y0 + 1, PLASMA_COLOR_HALO);
            plasma_put_pixel_safe(x0 - 2, y0 + 1, PLASMA_COLOR_HALO);
            plasma_put_pixel_safe(x0 + 2, y0 - 1, PLASMA_COLOR_HALO);
            plasma_put_pixel_safe(x0 - 2, y0 - 1, PLASMA_COLOR_HALO);
            plasma_put_pixel_safe(x0 + 1, y0 + 2, PLASMA_COLOR_HALO);
            plasma_put_pixel_safe(x0 - 1, y0 + 2, PLASMA_COLOR_HALO);
            plasma_put_pixel_safe(x0 + 1, y0 - 2, PLASMA_COLOR_HALO);
            plasma_put_pixel_safe(x0 - 1, y0 - 2, PLASMA_COLOR_HALO);
        } else if (level == 1) {
            /* Secondary branch */
            plasma_put_pixel_safe(x0, y0, PLASMA_COLOR_CORE);
            plasma_put_pixel_safe(x0 + 1, y0, PLASMA_COLOR_GLOW);
            plasma_put_pixel_safe(x0 - 1, y0, PLASMA_COLOR_GLOW);
            plasma_put_pixel_safe(x0, y0 + 1, PLASMA_COLOR_GLOW);
            plasma_put_pixel_safe(x0, y0 - 1, PLASMA_COLOR_GLOW);
            plasma_put_pixel_safe(x0 + 1, y0 + 1, PLASMA_COLOR_HALO);
            plasma_put_pixel_safe(x0 - 1, y0 + 1, PLASMA_COLOR_HALO);
            plasma_put_pixel_safe(x0 + 1, y0 - 1, PLASMA_COLOR_HALO);
            plasma_put_pixel_safe(x0 - 1, y0 - 1, PLASMA_COLOR_HALO);
        } else {
            /* Tertiary micro-filament: fine, delicate, crisp */
            plasma_put_pixel_safe(x0, y0, PLASMA_COLOR_GLOW);
            plasma_put_pixel_safe(x0 + 1, y0, PLASMA_COLOR_HALO);
            plasma_put_pixel_safe(x0, y0 + 1, PLASMA_COLOR_HALO);
        }

        if (x0 == x1 && y0 == y1) break;
        int e2 = 2 * err;
        if (e2 > -dy) {
            err -= dy;
            x0 += sx;
        }
        if (e2 < dx) {
            err += dx;
            y0 += sy;
        }
    }
}

static inline float plasma_frand(float min, float max) {
    return min + (float)rand() / (float)RAND_MAX * (max - min);
}

static void plasma_render_arc(plasma_pt_t start, plasma_pt_t end, float roughness, int depth, int level) {
    int num_pts = (1 << depth) + 1;
    plasma_pt_t pts[33];
    pts[0] = start;
    pts[num_pts - 1] = end;

    float disp = roughness;
    for (int step = (num_pts - 1) / 2; step > 0; step /= 2) {
        for (int i = step; i < num_pts; i += step * 2) {
            plasma_pt_t p1 = pts[i - step];
            plasma_pt_t p2 = pts[i + step];
            float mx = (p1.x + p2.x) * 0.5f;
            float my = (p1.y + p2.y) * 0.5f;
            float dx = p2.x - p1.x;
            float dy = p2.y - p1.y;
            float len = sqrtf(dx * dx + dy * dy);
            float nx = 0, ny = 1;
            if (len > 0.001f) {
                nx = -dy / len;
                ny = dx / len;
            }
            float offset = plasma_frand(-disp, disp);
            pts[i].x = mx + nx * offset;
            pts[i].y = my + ny * offset;
        }
        disp *= 0.53f;
    }

    for (int i = 0; i < num_pts - 1; i++) {
        plasma_draw_line((int)(pts[i].x + 0.5f), (int)(pts[i].y + 0.5f),
                         (int)(pts[i + 1].x + 0.5f), (int)(pts[i + 1].y + 0.5f), level);
    }

    /* Secondary branches from primary trunk (level 0 -> level 1) */
    if (level == 0) {
        int num_branches = 1 + ((rand() % 100) < 60 ? 1 : 0);
        for (int b = 0; b < num_branches; b++) {
            int b_idx = 4 + (rand() % (num_pts - 8));
            plasma_pt_t b_start = pts[b_idx];
            float cur_angle = atan2f(b_start.y - 120.0f, b_start.x - 120.0f);
            float branch_angle = cur_angle + plasma_frand(-0.35f, 0.35f);
            float reach = plasma_frand(0.85f, 1.0f);
            plasma_pt_t b_target = {
                120.0f + 114.0f * reach * cosf(branch_angle),
                120.0f + 114.0f * reach * sinf(branch_angle)
            };
            plasma_render_arc(b_start, b_target, roughness * 0.65f, 4, 1);
        }
    }

    /* Tertiary micro-filaments (level 1 -> level 2): delicate spiderweb cracks */
    if (level == 1 && (rand() % 100) < 65) {
        int m_idx = 3 + (rand() % (num_pts - 6));
        plasma_pt_t m_start = pts[m_idx];
        float cur_angle = atan2f(m_start.y - 120.0f, m_start.x - 120.0f);
        float micro_angle = cur_angle + plasma_frand(-0.45f, 0.45f);
        float micro_dist = plasma_frand(18.0f, 32.0f);
        plasma_pt_t m_target = {
            m_start.x + micro_dist * cosf(micro_angle),
            m_start.y + micro_dist * sinf(micro_angle)
        };
        plasma_render_arc(m_start, m_target, roughness * 0.45f, 3, 2);
    }
}

static plasma_pt_t plasma_find_boundary(float angle) {
    float ca = cosf(angle);
    float sa = sinf(angle);
    plasma_pt_t best = { 120.0f + 35.0f * ca, 120.0f + 35.0f * sa };
    for (int r = 16; r < 80; r += 2) {
        int tx = (int)(120.0f + r * ca);
        int ty = (int)(120.0f + r * sa);
        if (tx >= 0 && tx < HOR_RES && ty >= 0 && ty < VER_RES) {
            if (!is_btc_mask(tx, ty)) {
                best.x = (float)tx;
                best.y = (float)ty;
                break;
            }
        }
    }
    return best;
}

static void refresh_plasma(int init) {
    if (!plasma_inited || init) {
        memset(plasma_fb, 0, sizeof(plasma_fb));
        for (int i = 0; i < PLASMA_NUM_STREAMERS; i++) {
            float base_ang = (float)i * (2.0f * 3.14159265f / PLASMA_NUM_STREAMERS) + plasma_frand(-0.08f, 0.08f);
            plasma_streamers[i].angle = base_ang;
            plasma_streamers[i].target_angle = base_ang + plasma_frand(-0.35f, 0.35f);
            plasma_streamers[i].life = 10 + (rand() % 18);
            plasma_streamers[i].intensity = plasma_frand(0.85f, 1.0f);
        }
        plasma_inited = 1;
    }

    plasma_decay_buffer();

    /* Render 14 streamers covering all 360 degrees */
    for (int i = 0; i < PLASMA_NUM_STREAMERS; i++) {
        plasma_streamers[i].angle += (plasma_streamers[i].target_angle - plasma_streamers[i].angle) * 0.15f + plasma_frand(-0.03f, 0.03f);
        plasma_streamers[i].life--;
        if (plasma_streamers[i].life <= 0) {
            plasma_streamers[i].target_angle += plasma_frand(-0.5f, 0.5f);
            plasma_streamers[i].life = 8 + (rand() % 18);
            plasma_streamers[i].intensity = plasma_frand(0.85f, 1.0f);
        }

        plasma_pt_t p_start = plasma_find_boundary(plasma_streamers[i].angle);
        float rim_angle = plasma_streamers[i].angle + plasma_frand(-0.18f, 0.18f);
        plasma_pt_t p_target = {
            120.0f + 114.0f * cosf(rim_angle),
            120.0f + 114.0f * sinf(rim_angle)
        };

        /* Primary arc (level 0, depth 5 = 32 segments) */
        plasma_render_arc(p_start, p_target, 18.0f * plasma_streamers[i].intensity, 5, 0);

        /* Grounding impact flare on outer rim with lateral glass creep */
        int rx = (int)p_target.x;
        int ry = (int)p_target.y;
        plasma_put_pixel_safe(rx, ry, PLASMA_COLOR_CORE);
        plasma_put_pixel_safe(rx + 1, ry, PLASMA_COLOR_GLOW);
        plasma_put_pixel_safe(rx - 1, ry, PLASMA_COLOR_GLOW);
        plasma_put_pixel_safe(rx, ry + 1, PLASMA_COLOR_GLOW);
        plasma_put_pixel_safe(rx - 1, ry, PLASMA_COLOR_GLOW);
        plasma_put_pixel_safe(rx + 1, ry + 1, PLASMA_COLOR_HALO);
        plasma_put_pixel_safe(rx - 1, ry + 1, PLASMA_COLOR_HALO);
        plasma_put_pixel_safe(rx + 1, ry - 1, PLASMA_COLOR_HALO);
        plasma_put_pixel_safe(rx - 1, ry - 1, PLASMA_COLOR_HALO);

        /* Lateral glass surface creep */
        float tan_x = -sinf(rim_angle);
        float tan_y = cosf(rim_angle);
        for (int k = 1; k <= 3; k++) {
            plasma_put_pixel_safe((int)(rx + tan_x * k), (int)(ry + tan_y * k), PLASMA_COLOR_GLOW);
            plasma_put_pixel_safe((int)(rx - tan_x * k), (int)(ry - tan_y * k), PLASMA_COLOR_GLOW);
        }
    }

    /* Outer glass rim glow in #F7931A */
    for (int a = 0; a < 360; a++) {
        float rad = (float)a * 3.14159265f / 180.0f;
        int rx = (int)(120.0f + 114.0f * cosf(rad) + 0.5f);
        int ry = (int)(120.0f + 114.0f * sinf(rad) + 0.5f);
        if (rx >= 0 && rx < HOR_RES && ry >= 0 && ry < VER_RES) {
            plasma_fb[ry * HOR_RES + rx] = plasma_blend_add(plasma_fb[ry * HOR_RES + rx], 0x9300);
        }
    }

    /* Energized orange corona around the Bitcoin B perimeter */
    for (int y = 65; y <= 175; y++) {
        for (int x = 75; x <= 165; x++) {
            if (is_btc_border(x, y)) {
                plasma_fb[y * HOR_RES + x] = plasma_blend_add(plasma_fb[y * HOR_RES + x], PLASMA_COLOR_BORDER);
            }
        }
    }

    /* Crackling corona spark hotspots along the B edge (high-energy micro-arcs) */
    for (int s = 0; s < 12; s++) {
        float spark_angle = plasma_frand(0.0f, 6.28318f);
        plasma_pt_t sp = plasma_find_boundary(spark_angle);
        plasma_put_pixel_safe((int)sp.x, (int)sp.y, PLASMA_COLOR_SPARK);
        plasma_put_pixel_safe((int)sp.x + 1, (int)sp.y, PLASMA_COLOR_GLOW);
        plasma_put_pixel_safe((int)sp.x - 1, (int)sp.y, PLASMA_COLOR_GLOW);
        plasma_put_pixel_safe((int)sp.x, (int)sp.y + 1, PLASMA_COLOR_GLOW);
        plasma_put_pixel_safe((int)sp.x - 1, (int)sp.y, PLASMA_COLOR_GLOW);
    }

    /* Final mask: The interior of the Bitcoin B is solid jet black, and outside perimeter is black */
    for (int y = 0; y < VER_RES; y++) {
        for (int x = 0; x < HOR_RES; x++) {
            int dx = x - 120;
            int dy = y - 120;
            if (dx * dx + dy * dy > 115 * 115) {
                plasma_fb[y * HOR_RES + x] = 0;
            } else if (is_btc_mask(x, y)) {
                plasma_fb[y * HOR_RES + x] = 0;
            }
        }
    }

    /* Blit to /dev/fb0 */
    int fd = open(FB_DEV, O_WRONLY);
    if (fd >= 0) {
        lseek(fd, 0, SEEK_SET);
        size_t written = 0;
        size_t total = sizeof(plasma_fb);
        const uint8_t *ptr = (const uint8_t *)plasma_fb;
        while (written < total) {
            ssize_t wn = write(fd, ptr + written, total - written);
            if (wn <= 0) break;
            written += (size_t)wn;
        }
        close(fd);
    }
}


/* ── Page-file polling & Theme Sync Event Loop ────────────────────────── */

typedef enum {
    PG_SPLASH,
    PG_LIVE,
    PG_TUNER,
    PG_IP,
    PG_PARASITE,
    PG_PIZZA,
    PG_IXLOGO,
    PG_PLASMA,
    PG_DOOM,
    PG_WIFI_SETUP
} page_t;

static page_t parse_page(const char *s) {
    if (strcmp(s, "mujina-logo") == 0) return PG_SPLASH;
    if (strcmp(s, "nano3s") == 0) return PG_LIVE;
    if (strcmp(s, "nano3s-tuner") == 0) return PG_TUNER;
    if (strcmp(s, "ip") == 0) return PG_IP;
    if (strcmp(s, "parasite-logo") == 0) return PG_PARASITE;
    if (strcmp(s, "pizza") == 0) return PG_PIZZA;
    if (strcmp(s, "ixlogo") == 0) return PG_IXLOGO;
    if (strcmp(s, "plasma") == 0) return PG_PLASMA;
    if (strcmp(s, "wifi-setup") == 0) return PG_WIFI_SETUP;
    if (strcmp(s, "doom") == 0) return PG_DOOM;
    /* Diagnostics (nano3s-diag) removed; unrecognized names default to Splash */
    return PG_SPLASH;
}

static void run_event_loop(void) {
    char last_page[64] = "";
    ui_theme_t last_theme = (ui_theme_t)(-1);
    uint32_t last_refresh = now_ms() - REFRESH_INTERVAL_MS;
    uint32_t last_tick = now_ms();
    int was_doom = 0;
    int was_lvgl = 0;

    for (;;) {
        uint32_t now = now_ms();
        lv_tick_inc(now - last_tick);
        last_tick = now;

        char page[64] = "mujina-logo";
        FILE *pf = fopen(PAGE_FILE, "r");
        if (pf) {
            if (fgets(page, sizeof(page), pf)) {
                page[strcspn(page, "\r\n")] = '\0';
            }
            fclose(pf);
        }

        static uint32_t last_theme_check = 0;
        static ui_theme_t cached_theme = THEME_CYBERPUNK;
        if (now - last_theme_check >= 1000 || last_theme == (ui_theme_t)(-1)) {
            cached_theme = read_ui_theme();
            last_theme_check = now;
        }
        ui_theme_t theme = cached_theme;
        page_t pg = parse_page(page);
        if (pg == PG_PARASITE && theme != THEME_PARASITE) {
            pg = PG_SPLASH;
        }
        int page_changed = strcmp(page, last_page) != 0;
        int theme_changed = (theme != last_theme);

        if (was_doom && pg != PG_DOOM) {
            silence_buzzer();
        }
        was_doom = (pg == PG_DOOM);

        if (pg == PG_DOOM) {
            strcpy(last_page, page);
            last_theme = theme;
            was_lvgl = 0;
            usleep(POLL_INTERVAL_MS * 1000);
            continue;
        }

        if (pg == PG_IXLOGO) {
            refresh_ixlogo(now);
            strcpy(last_page, page);
            last_theme = theme;
            was_lvgl = 0;
            usleep(IXLOGO_FRAME_MS * 1000); /* 15 FPS: 66ms */
            continue;
        }

        if (pg == PG_PLASMA) {
            refresh_plasma(page_changed);
            strcpy(last_page, page);
            last_theme = theme;
            was_lvgl = 0;
            usleep(50 * 1000); /* 20 FPS Bitcoin Plasma */
            continue;
        }

        if (pg == PG_SPLASH) {
            refresh_splash_themed(theme, now);
            strcpy(last_page, page);
            last_theme = theme;
            was_lvgl = 0;
            usleep(ANIM_INTERVAL_MS * 1000); /* 25 FPS animated splash rings */
            continue;
        }

        if (pg == PG_PARASITE) {
            refresh_parasite_logo(now);
            strcpy(last_page, page);
            last_theme = theme;
            was_lvgl = 0;
            usleep(ANIM_INTERVAL_MS * 1000); /* 25 FPS animated Parasite chronograph */
            continue;
        }

        if (pg == PG_WIFI_SETUP) {
            refresh_wifi_setup_themed(theme, now);
            strcpy(last_page, page);
            last_theme = theme;
            was_lvgl = 0;
            usleep(50 * 1000); /* 20 FPS animated Bluetooth setup */
            continue;
        }

        if (page_changed || theme_changed || now - last_refresh >= REFRESH_INTERVAL_MS) {
            strcpy(last_page, page);
            last_theme = theme;
            last_refresh = now;

            if (pg == PG_IP) {
                refresh_ip_themed(theme);
                was_lvgl = 0;
            } else if (pg == PG_PIZZA) {
                if (page_changed) refresh_pizza();
                was_lvgl = 0;
            } else if (read_kv_file(NANO3S_LIVE_FILE)) {
                if (pg == PG_TUNER) refresh_tuner_themed(theme);
                else refresh_live_themed(theme);
                was_lvgl = 0;
            } else {
                refresh_splash_themed(theme, now);
                was_lvgl = 0;
            }
        }

        if (was_lvgl) {
            lv_timer_handler();
        }
        usleep(POLL_INTERVAL_MS * 1000);
    }
}

static void clear_framebuffer(void) {
    static uint8_t zeros[FB_BYTES];
    int fd = open(FB_DEV, O_WRONLY);
    if (fd < 0) return;
    lseek(fd, 0, SEEK_SET);
    size_t written = 0;
    while (written < sizeof(zeros)) {
        ssize_t n = write(fd, zeros + written, sizeof(zeros) - written);
        if (n <= 0) break;
        written += (size_t)n;
    }
    close(fd);
}

int nano3s_ui_run(void) {
    clear_framebuffer();
    init_ring_lut();
    init_ixlogo();
    lv_init();

    static lv_disp_draw_buf_t draw_buf;
    lv_disp_draw_buf_init(&draw_buf, draw_buf1, NULL, HOR_RES * 10);

    static lv_disp_drv_t disp_drv;
    lv_disp_drv_init(&disp_drv);
    disp_drv.hor_res = HOR_RES;
    disp_drv.ver_res = VER_RES;
    disp_drv.flush_cb = fb_flush_cb;
    disp_drv.draw_buf = &draw_buf;
    lv_disp_drv_register(&disp_drv);

    build_screens();
    refresh_splash_themed(read_ui_theme(), now_ms());

    run_event_loop();
    return 0;
}
