#!/usr/sbin/dtrace -s

#pragma D option switchrate=10hz

/* propolis$target:::uart_reg_read */
/* /arg1 == 0/ */
/* { */
/* 	/\* printf("%c", arg2); *\/ */
/* } */

/* propolis$target:::uart_reg_write */
/* /arg1 == 0/ */
/* { */
/* 	printf("%c", (char)arg2); */
/* } */

/* propolis$target:::uart_* */
/* { */
/* } */

/* propolis*:::lpc_pio_rw_read */
/* { */
/* 	printf("read\t@%2x\t'%2x'\n", arg0, (char)arg1); */
/* } */

/* propolis*:::lpc_pio_rw_write */
/* { */
/* 	printf("write\t@%2x\t'%2x'\n", arg0, (char)arg1); */
/* } */

/* propolis*:::source_buffer_wait_* */
/* { */
/* 	trace(timestamp); */
/* } */

/* propolis*:::source_buffer_wait_miss */
/* { */
/* 	printf("miss=%d/%d", arg1, arg2); */
/* } */

propolis*:::pio_in
/ arg0 == 0x3f8 /
{
	printf("%Y in[%C]\n", walltimestamp, (char)(arg2 & 0xff));
}

/* propolis*:::pio_in */
/* / arg0 != 0x3f8 / */
/* { */
/* 	printf("%Y other in (port %04X)\n", walltimestamp, arg0); */
/* } */

propolis*:::chardev_sock_*
{
}

propolis*:::chardev_sock_run_sink_write
{
	trace(arg0);
}
