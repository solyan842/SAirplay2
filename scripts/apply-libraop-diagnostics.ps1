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

Replace-Once -Path $raop -Label "pyatv-compatible PCM L16 fmtp" -Old @'
		case RAOP_PCM: {
			char buf[256];

			sprintf(buf,
					"m=audio 0 RTP/AVP 96\r\n"
					"a=rtpmap:96 L%d/%d/%d\r\n",
					p->sample_size, p->sample_rate, p->channels);
			strcat(sdp, buf);
			break;
		}
'@ -New @'
		case RAOP_PCM: {
			char buf[256];

			if (getenv("SAIRPLAY_PCM_L16_FMTP") && *getenv("SAIRPLAY_PCM_L16_FMTP")) {
				sprintf(buf,
						"m=audio 0 RTP/AVP 96\r\n"
						"a=rtpmap:96 L%d/%d/%d\r\n"
						"a=fmtp:96 %d 0 %d 40 10 14 %d 255 0 0 %d\r\n",
						p->sample_size, p->sample_rate, p->channels,
						p->chunk_len, p->sample_size, p->channels, p->sample_rate);
				LOG_INFO("[SAIRPLAY-DIAG] pcm_l16_sdp=pyatv-compatible");
			} else {
				sprintf(buf,
						"m=audio 0 RTP/AVP 96\r\n"
						"a=rtpmap:96 L%d/%d/%d\r\n",
						p->sample_size, p->sample_rate, p->channels);
			}
			strcat(sdp, buf);
			break;
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
	packet->timestamp = htonl(p->head_ts);
	packet->ssrc = htonl(p->ssrc);

	memcpy((uint8_t*) packet + sizeof(rtp_audio_pkt_t), encoded, size);
'@ -New @'
	packet->timestamp = htonl(p->head_ts);
	packet->ssrc = htonl(p->ssrc);

	{
		static bool sairplay_diag_rtp_header_logged = false;
		if (!sairplay_diag_rtp_header_logged) {
			LOG_INFO("[SAIRPLAY-DIAG] first_rtp_header seq=%u rtptime=%u marker=%u payload=%d",
					 p->seq_number, (uint32_t) p->head_ts,
					 (unsigned) ((packet->hdr.type & 0x80) != 0), size);
			sairplay_diag_rtp_header_logged = true;
		}
	}

	memcpy((uint8_t*) packet + sizeof(rtp_audio_pkt_t), encoded, size);
'@

Write-Host "Applied SAirplay2 one-shot libraop runtime diagnostics."
