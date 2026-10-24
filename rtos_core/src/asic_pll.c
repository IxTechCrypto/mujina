/*
 * A3197S PLL programming: MHz<->register-encoding table lookups, the
 * blocking one-second-per-step ramp, and a3197s_pll_apply() (formerly
 * asic_set_miner_pll()) -- the PLL0-3 register write sequence bracketed
 * by the SmartSpeed SS_CTRL PRE/MID/COMMIT handshake (see
 * asic_smartspeed.c's a3197s_smartspeed_set_ctrl()). The call sequence
 * and every register write below are unchanged from the pre-split
 * function; only the 3 repeated SS_CTRL+SPD_SYNC_CFG write pairs were
 * factored into a shared helper.
 */

#include <stdint.h>
#include <stdio.h>
#include "util.h"
#include "pll_cpm_table.h"
#include "asic_engine.h"
#include "asic_internal.h"

a3197s_pll_state_t g_pll_state;

static uint32_t get_cpm_pll(uint32_t freq)
{
	uint32_t i;
	for (i = 0; i < PLL_TABLE_ROWS; i++)
	{
		if (freq <= cpm_table[i][0])
			return cpm_table[i][1];
	}

	return 0;
}

uint32_t get_cpm_freq(uint32_t pll)
{
	uint32_t i, freq = 0;

	pll = (pll & 0x9fffffff) | 1;
	for (i = 0; i < PLL_TABLE_ROWS; i++)
	{
		if (pll == cpm_table[i][1])
		{
			return cpm_table[i][0];
		}
	}

	return 0;
}

/* Per-chip MHz offsets added to every PLL domain of that chip. All zero
 * (the default) keeps a3197s_pll_apply() on its original broadcast path. */
int16_t g_pll_chip_offset[HBOARD_COUNT][PLL_CHIP_OFFSET_MAX];

static int pll_offsets_active(uint8_t miner_id)
{
	int i;

	for (i = 0; i < PLL_CHIP_OFFSET_MAX; i++)
		if (g_pll_chip_offset[miner_id][i] != 0)
			return 1;
	return 0;
}

static uint32_t pll_offset_freq(uint32_t base, int16_t offset)
{
	int32_t f = (int32_t)base + offset;

	return (uint32_t)(f < PLL_CHIP_MIN_MHZ ? PLL_CHIP_MIN_MHZ : f);
}

/* Sets g_pll_state.set_pll[miner_id][0][0..3] and applies it via
 * a3197s_pll_apply(), which broadcasts to the whole chain (or addresses
 * each chip in turn while per-chip offsets are set). */
void asic_apply_pll_freq(uint8_t miner_id, const uint32_t pll_freq[4])
{
	int k;

	for (k = 0; k < 4; k++)
		g_pll_state.set_pll[miner_id][0][k] = (uint16_t)pll_freq[k];

	a3197s_pll_apply(miner_id, 0);
}

/* Blocking PLL ramp: steps PLL0 from init_freq to target_freq in
 * PLL_RAMP_STEP_MHZ increments, once per second. Each step, PLL1-3 are
 * set to PLL0 + domain_interval*{1,2,3}. */
#define PLL_RAMP_STEP_MHZ 10

void asic_ramp_pll_freq(uint8_t miner_id, uint16_t init_freq, uint16_t target_freq, uint16_t domain_interval)
{
	int span = (int)target_freq - (int)init_freq;
	int direction = (span >= 0) ? 1 : -1;
	unsigned abs_span = (unsigned)((span >= 0) ? span : -span);
	unsigned m = abs_span / PLL_RAMP_STEP_MHZ;
	unsigned n = abs_span % PLL_RAMP_STEP_MHZ;
	unsigned step;
	uint32_t pll_freq[4];

	if (target_freq == init_freq)
		return;

	for (step = 0; step <= m; step++) {
		uint16_t freq_step = (uint16_t)(init_freq + step * PLL_RAMP_STEP_MHZ * direction);

		pll_freq[0] = freq_step;
		pll_freq[1] = freq_step + domain_interval;
		pll_freq[2] = freq_step + domain_interval * 2;
		pll_freq[3] = freq_step + domain_interval * 3;
		asic_apply_pll_freq(miner_id, pll_freq);
		delay_ms(1000);
	}

	if (n != 0) {
		pll_freq[0] = target_freq;
		pll_freq[1] = target_freq + domain_interval;
		pll_freq[2] = target_freq + domain_interval * 2;
		pll_freq[3] = target_freq + domain_interval * 3;
		asic_apply_pll_freq(miner_id, pll_freq);
		delay_ms(1000);
	}
}

/* The PLL write sequence for whatever chip (or broadcast) is selected. */
static void pll_write_sequence(uint8_t miner_id, uint32_t freq0, uint32_t freq1,
			       uint32_t freq2, uint32_t freq3)
{
	uint32_t tmp32;

	/* SS_CTRL handshake bracketing this PLL step. */
	a3197s_smartspeed_set_ctrl(miner_id, SS_CTRL_RAMP_PRE);

	tmp32 = get_cpm_pll(freq1);
	a3197s_set_reg(miner_id, REG_PLL1_FREQ, tmp32);
	tmp32 = get_cpm_pll(freq2);
	a3197s_set_reg(miner_id, REG_PLL2_FREQ, tmp32);
	tmp32 = get_cpm_pll(freq3);
	a3197s_set_reg(miner_id, REG_PLL3_FREQ, tmp32);

	a3197s_smartspeed_set_ctrl(miner_id, SS_CTRL_RAMP_MID);

	tmp32 = get_cpm_pll(freq0);
	a3197s_set_reg(miner_id, REG_PLL0_FREQ, tmp32);

	a3197s_smartspeed_set_ctrl(miner_id, SS_CTRL_RAMP_COMMIT);

	tmp32 = ROLLTIME_TIMEOUT_CONST / freq3;
	a3197s_set_reg(miner_id, REG_ROLLTIME, tmp32);
}

void a3197s_pll_apply(uint8_t miner_id, uint16_t asic_id)
{
	const uint16_t *base = g_pll_state.set_pll[miner_id][0];
	uint16_t chip_select_id;

	(void)asic_id;
	chip_select_id = asic_get_chip_select();

	if (!pll_offsets_active(miner_id)) {
		a3197s_select_chip(miner_id, CHIP_ADDR_BROADCAST);
		pll_write_sequence(miner_id, base[0], base[1], base[2], base[3]);
	} else {
		/* Each chip gets the same handshake and writes the broadcast
		 * path sends, addressed to it alone, at its own clocks. */
		uint16_t chip;

		for (chip = 0; chip < PLL_CHIP_OFFSET_MAX; chip++) {
			int16_t off = g_pll_chip_offset[miner_id][chip];

			a3197s_select_chip(miner_id, chip);
			pll_write_sequence(miner_id, pll_offset_freq(base[0], off),
				pll_offset_freq(base[1], off), pll_offset_freq(base[2], off),
				pll_offset_freq(base[3], off));
		}
		a3197s_select_chip(miner_id, CHIP_ADDR_BROADCAST);
	}
	delay_ms(1);

	/* Read-only diagnostic: logs the LOCK/LOCK_IP bits inside the PLLxL1
	 * registers just written, read back from chip 0 as a representative
	 * sample. Not used to gate anything. */
	if (g_lock_diag_fp && g_lock_diag_count < PLL_LOCK_DIAG_MAX_LINES) {
		uint32_t pll0_raw, pll1_raw, pll2_raw, pll3_raw;

		a3197s_select_chip(miner_id, 0);
		pll0_raw = a3197s_get_reg(miner_id, REG_PLL0_FREQ);
		pll1_raw = a3197s_get_reg(miner_id, REG_PLL1_FREQ);
		pll2_raw = a3197s_get_reg(miner_id, REG_PLL2_FREQ);
		pll3_raw = a3197s_get_reg(miner_id, REG_PLL3_FREQ);

		fprintf(g_lock_diag_fp,
			"PLL_LOCK_DIAG chip0 pll0=0x%08x(lock=%d,lock_ip=%d) "
			"pll1=0x%08x(lock=%d,lock_ip=%d) pll2=0x%08x(lock=%d,lock_ip=%d) "
			"pll3=0x%08x(lock=%d,lock_ip=%d)\n",
			pll0_raw, !!(pll0_raw & PLL_LOCK_MASK), !!(pll0_raw & PLL_LOCK_IP_MASK),
			pll1_raw, !!(pll1_raw & PLL_LOCK_MASK), !!(pll1_raw & PLL_LOCK_IP_MASK),
			pll2_raw, !!(pll2_raw & PLL_LOCK_MASK), !!(pll2_raw & PLL_LOCK_IP_MASK),
			pll3_raw, !!(pll3_raw & PLL_LOCK_MASK), !!(pll3_raw & PLL_LOCK_IP_MASK));
		fflush(g_lock_diag_fp);
		g_lock_diag_count++;
	}

	a3197s_select_chip(miner_id, chip_select_id);
}

/* Periodic REG_ROLLTIME refresh, computed from freq3. Deliberately
 * separate from a3197s_pll_apply() so it doesn't also rewrite the
 * PLL0-3 dividers. */
void asic_refresh_rolltime(uint8_t miner_id, uint16_t freq3)
{
	uint16_t chip_select_id = asic_get_chip_select();
	uint32_t tmp32 = ROLLTIME_TIMEOUT_CONST / freq3;

	if (!pll_offsets_active(miner_id)) {
		a3197s_select_chip(miner_id, CHIP_ADDR_BROADCAST);
		a3197s_set_reg(miner_id, REG_ROLLTIME, tmp32);
	} else {
		uint16_t chip;

		for (chip = 0; chip < PLL_CHIP_OFFSET_MAX; chip++) {
			a3197s_select_chip(miner_id, chip);
			a3197s_set_reg(miner_id, REG_ROLLTIME, ROLLTIME_TIMEOUT_CONST /
				pll_offset_freq(freq3, g_pll_chip_offset[miner_id][chip]));
		}
	}
	a3197s_select_chip(miner_id, chip_select_id);
}

/* Moves the per-chip offsets toward target, at most PLL_RAMP_STEP_MHZ per
 * chip per step, one second apart (same pacing as asic_ramp_pll_freq()). */
void asic_set_pll_chip_offsets(uint8_t miner_id, const int16_t target[PLL_CHIP_OFFSET_MAX])
{
	int moving = 1;

	while (moving) {
		int i;

		moving = 0;
		for (i = 0; i < PLL_CHIP_OFFSET_MAX; i++) {
			int16_t *cur = &g_pll_chip_offset[miner_id][i];
			int16_t d = (int16_t)(target[i] - *cur);

			if (d > PLL_RAMP_STEP_MHZ)
				d = PLL_RAMP_STEP_MHZ;
			else if (d < -PLL_RAMP_STEP_MHZ)
				d = -PLL_RAMP_STEP_MHZ;
			if (d != 0) {
				*cur = (int16_t)(*cur + d);
				moving = 1;
			}
		}
		if (moving) {
			/* Once every offset reaches zero this is the broadcast path. */
			a3197s_pll_apply(miner_id, 0);
			delay_ms(1000);
		}
	}
}
