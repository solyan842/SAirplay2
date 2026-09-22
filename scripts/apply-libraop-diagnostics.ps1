param(
    [Parameter(Mandatory = $true)]
    [string]$SourceRoot
)

$ErrorActionPreference = "Stop"

function Replace-Once {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$Old,
        [Parameter(Mandatory = $true)][string]$New,
        [Parameter(Mandatory = $true)][string]$Label
    )

    $text = [System.IO.File]::ReadAllText($Path).Replace("`r`n", "`n")
    $oldNorm = $Old.Replace("`r`n", "`n")
    $newNorm = $New.Replace("`r`n", "`n")

    $first = $text.IndexOf($oldNorm, [System.StringComparison]::Ordinal)
    if ($first -lt 0) {
        throw "Diagnostic patch anchor not found: $Label"
    }
    $second = $text.IndexOf($oldNorm, $first + $oldNorm.Length, [System.StringComparison]::Ordinal)
    if ($second -ge 0) {
        throw "Diagnostic patch anchor is not unique: $Label"
    }

    $patched = $text.Substring(0, $first) + $newNorm + $text.Substring($first + $oldNorm.Length)
    [System.IO.File]::WriteAllText($Path, $patched, [System.Text.UTF8Encoding]::new($false))
}

$cliraop = Join-Path $SourceRoot "src/cliraop.c"
$raop = Join-Path $SourceRoot "src/raop_client.c"

Replace-Once -Path $raop -Label "RECORD RTP timeline diagnostic" -Old @'
	if (!rtspcl_record(p->rtspcl, p->seq_number + 1, NTP2TS(raopcl_get_ntp(NULL), p->sample_rate), kd)) goto erexit;
'@ -New @'
	{
		uint16_t sairplay_record_seq = p->seq_number + 1;
		uint32_t sairplay_record_ts = NTP2TS(raopcl_get_ntp(NULL), p->sample_rate);
		LOG_INFO("[SAIRPLAY-DIAG] record_rtp seq=%u rtptime=%u",
				 sairplay_record_seq, sairplay_record_ts);
		if (!rtspcl_record(p->rtspcl, sairplay_record_seq, sairplay_record_ts, kd)) goto erexit;
	}
'@

Replace-Once -Path $raop -Label "relative RTP clock state" -Old @'
	uint64_t head_ts, pause_ts, start_ts, first_ts;
'@ -New @'
	uint64_t head_ts, pause_ts, start_ts, first_ts;
	uint64_t sairplay_rtp_base;
'@

Replace-Once -Path $raop -Label "relative RTP clock reset" -Old @'
	p->head_ts = p->pause_ts = p->start_ts = p->first_ts = 0;
'@ -New @'
	p->head_ts = p->pause_ts = p->start_ts = p->first_ts = 0;
	p->sairplay_rtp_base = 0;
'@

Replace-Once -Path $raop -Label "relative RTP RECORD clock" -Old @'
	{
		uint16_t sairplay_record_seq = p->seq_number + 1;
		uint32_t sairplay_record_ts = NTP2TS(raopcl_get_ntp(NULL), p->sample_rate);
		LOG_INFO("[SAIRPLAY-DIAG] record_rtp seq=%u rtptime=%u",
				 sairplay_record_seq, sairplay_record_ts);
		if (!rtspcl_record(p->rtspcl, sairplay_record_seq, sairplay_record_ts, kd)) goto erexit;
	}
'@ -New @'
	{
		uint16_t sairplay_record_seq = p->seq_number + 1;
		uint64_t sairplay_record_abs = NTP2TS(raopcl_get_ntp(NULL), p->sample_rate);
		uint32_t sairplay_record_ts = (uint32_t) sairplay_record_abs;

		if (getenv("SAIRPLAY_RELATIVE_RTP_CLOCK") && *getenv("SAIRPLAY_RELATIVE_RTP_CLOCK")) {
			uint64_t latency = raopcl_latency(p);
			p->sairplay_rtp_base = sairplay_record_abs > latency
				? sairplay_record_abs - latency : 0;
			sairplay_record_ts = (uint32_t) (sairplay_record_abs - p->sairplay_rtp_base);
			LOG_INFO("[SAIRPLAY-DIAG] relative_rtp_base=%" PRIu64 " record_abs=%" PRIu64 " record_wire=%u",
					 p->sairplay_rtp_base, sairplay_record_abs, sairplay_record_ts);
		}

		LOG_INFO("[SAIRPLAY-DIAG] record_rtp seq=%u rtptime=%u",
				 sairplay_record_seq, sairplay_record_ts);
		if (!rtspcl_record(p->rtspcl, sairplay_record_seq, sairplay_record_ts, kd)) goto erexit;
	}
'@

Replace-Once -Path $raop -Label "relative RTP audio timestamp" -Old @'
	packet->timestamp = htonl(p->head_ts);
	packet->ssrc = htonl(p->ssrc);
'@ -New @'
	{
		uint32_t sairplay_wire_ts = (uint32_t) p->head_ts;
		if (p->sairplay_rtp_base) {
			sairplay_wire_ts = (uint32_t) (p->head_ts - p->sairplay_rtp_base);
		}
		packet->timestamp = htonl(sairplay_wire_ts);
	}
	packet->ssrc = htonl(p->ssrc);
'@

Replace-Once -Path $raop -Label "relative RTP sync fields" -Old @'
	rsp.rtp_timestamp = htonl(timestamp);
	rsp.rtp_timestamp_latency = htonl(timestamp - raopcld->latency_frames);
'@ -New @'
	if (raopcld->sairplay_rtp_base) {
		rsp.rtp_timestamp = htonl((uint32_t) (timestamp - raopcld->sairplay_rtp_base));
		rsp.rtp_timestamp_latency = htonl((uint32_t) (
			timestamp - raopcld->latency_frames - raopcld->sairplay_rtp_base));
	} else {
		rsp.rtp_timestamp = htonl(timestamp);
		rsp.rtp_timestamp_latency = htonl(timestamp - raopcld->latency_frames);
	}
'@

Replace-Once -Path $cliraop -Label "cliraop first accepted/read PCM" -Old @'
		if (status == PLAYING && raopcl_accept_frames(raopcl)) {
			n = read(infile, buf, DEFAULT_FRAMES_PER_CHUNK * 4);
			if (!n)	continue;
			raopcl_send_chunk(raopcl, buf, n / 4, &playtime);
			frames += n / 4;
		}
'@ -New @'
		if (status == PLAYING && raopcl_accept_frames(raopcl)) {
			static bool sairplay_diag_accept_logged = false;
			static bool sairplay_diag_pcm_logged = false;

			if (!sairplay_diag_accept_logged) {
				LOG_INFO("[SAIRPLAY-DIAG] accept_frames=true");
				sairplay_diag_accept_logged = true;
			}

			n = read(infile, buf, DEFAULT_FRAMES_PER_CHUNK * 4);
			if (n > 0 && !sairplay_diag_pcm_logged) {
				LOG_INFO("[SAIRPLAY-DIAG] stdin_pcm_read bytes=%d frames=%d", n, n / 4);
				sairplay_diag_pcm_logged = true;
			}
			if (!n)	continue;
			raopcl_send_chunk(raopcl, buf, n / 4, &playtime);
			frames += n / 4;
		}
'@

Replace-Once -Path $raop -Label "SETUP receiver ports" -Old @'
	if (!p->rtp_ports.audio.rport || !p->rtp_ports.ctrl.rport) {
		LOG_ERROR("[%p]: missing a RTP port in response", p);
		rc = false;
	} else if (!p->rtp_ports.time.rport) {
		LOG_INFO("[%p]: missing timing port, will get it later", p);
	}

	return rc;
'@ -New @'
	if (!p->rtp_ports.audio.rport || !p->rtp_ports.ctrl.rport) {
		LOG_ERROR("[%p]: missing a RTP port in response", p);
		rc = false;
	} else if (!p->rtp_ports.time.rport) {
		LOG_INFO("[%p]: missing timing port, will get it later", p);
	}

	LOG_INFO("[SAIRPLAY-DIAG] setup_ports audio_server=%u control=%u timing=%u",
			 p->rtp_ports.audio.rport, p->rtp_ports.ctrl.rport, p->rtp_ports.time.rport);

	return rc;
'@

Replace-Once -Path $raop -Label "first send_chunk" -Old @'
	pthread_mutex_lock(&p->mutex);

	/*
	 Move to streaming state only when really flushed. In most cases, this is
'@ -New @'
	pthread_mutex_lock(&p->mutex);

	{
		static bool sairplay_diag_send_chunk_logged = false;
		if (!sairplay_diag_send_chunk_logged) {
			LOG_INFO("[SAIRPLAY-DIAG] send_chunk_first frames=%d state=%d audio_fd=%d audio_server=%u",
					 frames, p->state, p->rtp_ports.audio.fd, p->rtp_ports.audio.rport);
			sairplay_diag_send_chunk_logged = true;
		}
	}

	/*
	 Move to streaming state only when really flushed. In most cases, this is
'@

Replace-Once -Path $raop -Label "first UDP send outcome" -Old @'
	if (FD_ISSET(p->rtp_ports.audio.fd, &wfds)) {
		n = sendto(p->rtp_ports.audio.fd, (void*) packet, + size, 0, (void*) &addr, sizeof(addr));
		if (n != size) {
			LOG_DEBUG("[%p]: error sending audio packet", p);
			ret = false;
			p->sane.audio.send++;
		}
		else p->sane.audio.send = 0;
		p->sane.audio.avail = 0;
	}
	else {
		LOG_DEBUG("[%p]: audio socket unavailable", p);
		ret = false;
		p->sane.audio.avail++;
	}
'@ -New @'
	{
		static bool sairplay_diag_sendto_logged = false;

		if (FD_ISSET(p->rtp_ports.audio.fd, &wfds)) {
			n = sendto(p->rtp_ports.audio.fd, (void*) packet, + size, 0, (void*) &addr, sizeof(addr));
			if (!sairplay_diag_sendto_logged) {
				LOG_INFO("[SAIRPLAY-DIAG] udp_sendto_first bytes=%d expected=%d audio_server=%u",
						 (int) n, size, p->rtp_ports.audio.rport);
				sairplay_diag_sendto_logged = true;
			}
			if (n != size) {
				LOG_DEBUG("[%p]: error sending audio packet", p);
				ret = false;
				p->sane.audio.send++;
			}
			else p->sane.audio.send = 0;
			p->sane.audio.avail = 0;
		}
		else {
			if (!sairplay_diag_sendto_logged) {
				LOG_INFO("[SAIRPLAY-DIAG] udp_sendto_first skipped=socket_unavailable audio_server=%u",
						 p->rtp_ports.audio.rport);
				sairplay_diag_sendto_logged = true;
			}
			LOG_DEBUG("[%p]: audio socket unavailable", p);
			ret = false;
			p->sane.audio.avail++;
		}
	}
'@

Replace-Once -Path $raop -Label "first RTP header timeline" -Old @'
	{
		uint32_t sairplay_wire_ts = (uint32_t) p->head_ts;
		if (p->sairplay_rtp_base) {
			sairplay_wire_ts = (uint32_t) (p->head_ts - p->sairplay_rtp_base);
		}
		packet->timestamp = htonl(sairplay_wire_ts);
	}
	packet->ssrc = htonl(p->ssrc);

	memcpy((uint8_t*) packet + sizeof(rtp_audio_pkt_t), encoded, size);
'@ -New @'
	{
		uint32_t sairplay_wire_ts = (uint32_t) p->head_ts;
		if (p->sairplay_rtp_base) {
			sairplay_wire_ts = (uint32_t) (p->head_ts - p->sairplay_rtp_base);
		}
		packet->timestamp = htonl(sairplay_wire_ts);
	}
	packet->ssrc = htonl(p->ssrc);

	{
		static bool sairplay_diag_rtp_header_logged = false;
		if (!sairplay_diag_rtp_header_logged) {
			LOG_INFO("[SAIRPLAY-DIAG] first_rtp_header seq=%u rtptime=%u marker=%u payload=%d",
					 p->seq_number, ntohl(packet->timestamp),
					 (unsigned) ((packet->hdr.type & 0x80) != 0), size);
			sairplay_diag_rtp_header_logged = true;
		}
	}

	memcpy((uint8_t*) packet + sizeof(rtp_audio_pkt_t), encoded, size);
'@

Write-Host "Applied SAirplay2 one-shot libraop runtime diagnostics."
